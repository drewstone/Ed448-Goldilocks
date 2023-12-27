use std::borrow::Borrow;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Mul, MulAssign, Neg, Sub, SubAssign};

use crate::constants::BASEPOINT_ORDER;
use crate::curve::edwards::affine::AffinePoint;
use crate::curve::montgomery::montgomery::MontgomeryPoint; // XXX: need to fix this path
use crate::curve::scalar_mul::variable_base;
use crate::curve::twedwards::extended::ExtendedPoint as TwistedExtendedPoint;
use crate::curve::{iso448, map_to_curve_elligator2};
use crate::field::{FieldElement, Scalar};
use elliptic_curve::hash2curve::{ExpandMsg, ExpandMsgXof, Expander, FromOkm};
use elliptic_curve::{
    generic_array::{
        typenum::{U57, U84},
        GenericArray,
    },
    group::{Group, GroupEncoding},
};
use rand_core::RngCore;
use subtle::{Choice, ConditionallyNegatable, ConditionallySelectable, ConstantTimeEq, CtOption};

/// The default hash to curve domain separation tag
pub const DEFAULT_HASH_TO_CURVE_SUITE: &[u8] = b"edwards448_XOF:SHAKE256_ELL2_RO_";
/// The default encode to curve domain separation tag
pub const DEFAULT_ENCODE_TO_CURVE_SUITE: &[u8] = b"edwards448_XOF:SHAKE256_ELL2_NU_";

#[allow(non_snake_case)]

/// Represent points on the (untwisted) edwards curve using Extended Homogenous Projective Co-ordinates
/// (x, y) -> (X/Z, Y/Z, Z, T)
/// a = 1, d = -39081
/// XXX: Make this more descriptive
/// Should this be renamed to EdwardsPoint so that we are consistent with Dalek crypto? Necessary as ExtendedPoint is not regular lingo?
#[derive(Copy, Clone, Debug)]
pub struct ExtendedPoint {
    pub(crate) X: FieldElement,
    pub(crate) Y: FieldElement,
    pub(crate) Z: FieldElement,
    pub(crate) T: FieldElement,
}

impl ConditionallySelectable for ExtendedPoint {
    fn conditional_select(a: &Self, b: &Self, choice: Choice) -> Self {
        ExtendedPoint {
            X: FieldElement::conditional_select(&a.X, &b.X, choice),
            Y: FieldElement::conditional_select(&a.Y, &b.Y, choice),
            Z: FieldElement::conditional_select(&a.Z, &b.Z, choice),
            T: FieldElement::conditional_select(&a.T, &b.T, choice),
        }
    }
}

pub struct CompressedEdwardsY(pub [u8; 57]);

impl CompressedEdwardsY {
    pub fn decompress(&self) -> CtOption<ExtendedPoint> {
        // Safe to unwrap here as the underlying data structure is a slice
        let (sign, b) = self.0.split_last().unwrap();

        let mut y_bytes: [u8; 56] = [0; 56];
        y_bytes.copy_from_slice(&b);

        // Recover x using y
        let y = FieldElement::from_bytes(&y_bytes);
        let yy = y.square();
        let dyy = FieldElement::EDWARDS_D * yy;
        let numerator = FieldElement::ONE - yy;
        let denominator = FieldElement::ONE - dyy;

        let (mut x, is_res) = FieldElement::sqrt_ratio(&numerator, &denominator);

        // Compute correct sign of x
        let compressed_sign_bit = Choice::from(sign >> 7);
        let is_negative = x.is_negative();
        x.conditional_negate(compressed_sign_bit ^ is_negative);

        CtOption::new(AffinePoint { x, y }.to_extended(), is_res)
    }
}

impl ConstantTimeEq for ExtendedPoint {
    fn ct_eq(&self, other: &Self) -> Choice {
        let XZ = self.X * other.Z;
        let ZX = self.Z * other.X;

        let YZ = self.Y * other.Z;
        let ZY = self.Z * other.Y;

        (XZ.ct_eq(&ZX)) & (YZ.ct_eq(&ZY))
    }
}

impl PartialEq for ExtendedPoint {
    fn eq(&self, other: &ExtendedPoint) -> bool {
        self.ct_eq(other).into()
    }
}
impl Eq for ExtendedPoint {}

impl Default for ExtendedPoint {
    fn default() -> ExtendedPoint {
        ExtendedPoint::IDENTITY
    }
}

impl Group for ExtendedPoint {
    type Scalar = Scalar;

    fn random(mut rng: impl RngCore) -> Self {
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self::hash_with_defaults(&bytes)
    }

    fn identity() -> Self {
        Self::IDENTITY
    }

    fn generator() -> Self {
        Self::GENERATOR
    }

    fn is_identity(&self) -> Choice {
        self.ct_eq(&Self::IDENTITY)
    }

    fn double(&self) -> Self {
        self.double()
    }
}

impl GroupEncoding for ExtendedPoint {
    type Repr = GenericArray<u8, U57>;

    fn from_bytes(bytes: &Self::Repr) -> CtOption<Self> {
        let mut value = [0u8; 57];
        value.copy_from_slice(bytes);
        CompressedEdwardsY(value).decompress()
    }

    fn from_bytes_unchecked(bytes: &Self::Repr) -> CtOption<Self> {
        let mut value = [0u8; 57];
        value.copy_from_slice(bytes);
        CompressedEdwardsY(value).decompress()
    }

    fn to_bytes(&self) -> Self::Repr {
        Self::Repr::clone_from_slice(&self.compress().0)
    }
}

impl ExtendedPoint {
    /// Identity point
    pub const IDENTITY: Self = Self {
        X: FieldElement::ZERO,
        Y: FieldElement::ONE,
        Z: FieldElement::ONE,
        T: FieldElement::ZERO,
    };

    /// Generator for the prime subgroup
    pub const GENERATOR: Self = crate::GOLDILOCKS_BASE_POINT;

    pub fn to_montgomery(&self) -> MontgomeryPoint {
        // u = y^2 * [(1-dy^2)/(1-y^2)]

        let affine = self.to_affine();

        let yy = affine.y.square();
        let dyy = FieldElement::EDWARDS_D * yy;

        let u = yy * (FieldElement::ONE - dyy) * (FieldElement::ONE - yy).invert();

        MontgomeryPoint(u.to_bytes())
    }

    /// Generic scalar multiplication to compute s*P
    pub fn scalar_mul(&self, scalar: &Scalar) -> Self {
        // Compute floor(s/4)
        let mut scalar_div_four = scalar.clone();
        scalar_div_four.div_by_four();

        // Use isogeny and dual isogeny to compute phi^-1((s/4) * phi(P))
        let partial_result = variable_base(&self.to_twisted(), &scalar_div_four).to_untwisted();
        // Add partial result to (scalar mod 4) * P
        partial_result.add(&self.scalar_mod_four(&scalar))
    }

    /// Returns (scalar mod 4) * P in constant time
    pub fn scalar_mod_four(&self, scalar: &Scalar) -> Self {
        // Compute compute (scalar mod 4)
        let s_mod_four = scalar[0] & 3;

        // Compute all possible values of (scalar mod 4) * P
        let zero_p = ExtendedPoint::IDENTITY;
        let one_p = self.clone();
        let two_p = one_p.double();
        let three_p = two_p.add(self);

        // Under the reasonable assumption that `==` is constant time
        // Then the whole function is constant time.
        // This should be cheaper than calling double_and_add or a scalar mul operation
        // as the number of possibilities are so small.
        // XXX: This claim has not been tested (although it sounds intuitive to me)
        let mut result = ExtendedPoint::IDENTITY;
        result.conditional_assign(&zero_p, Choice::from((s_mod_four == 0) as u8));
        result.conditional_assign(&one_p, Choice::from((s_mod_four == 1) as u8));
        result.conditional_assign(&two_p, Choice::from((s_mod_four == 2) as u8));
        result.conditional_assign(&three_p, Choice::from((s_mod_four == 3) as u8));

        result
    }

    // Standard compression; store Y and sign of X
    // XXX: This needs more docs and is `compress` the conventional function name? I think to_bytes/encode is?
    pub fn compress(&self) -> CompressedEdwardsY {
        let affine = self.to_affine();

        let affine_x = affine.x;
        let affine_y = affine.y;

        let mut compressed_bytes = [0u8; 57];

        let sign = affine_x.is_negative().unwrap_u8();

        let y_bytes = affine_y.to_bytes();
        for i in 0..y_bytes.len() {
            compressed_bytes[i] = y_bytes[i]
        }
        *compressed_bytes.last_mut().unwrap() = (sign as u8) << 7;
        CompressedEdwardsY(compressed_bytes)
    }

    //https://iacr.org/archive/asiacrypt2008/53500329/53500329.pdf (3.1)
    // These formulas are unified, so for now we can use it for doubling. Will refactor later for speed
    pub fn add(&self, other: &ExtendedPoint) -> Self {
        let aXX = self.X * other.X; // aX1X2
        let dTT = FieldElement::EDWARDS_D * self.T * other.T; // dT1T2
        let ZZ = self.Z * other.Z; // Z1Z2
        let YY = self.Y * other.Y;

        let X = {
            let x_1 = (self.X * other.Y) + (self.Y * other.X);
            let x_2 = ZZ - dTT;
            x_1 * x_2
        };
        let Y = {
            let y_1 = YY - aXX;
            let y_2 = ZZ + dTT;
            y_1 * y_2
        };

        let T = {
            let t_1 = YY - aXX;
            let t_2 = (self.X * other.Y) + (self.Y * other.X);
            t_1 * t_2
        };

        let Z = { (ZZ - dTT) * (ZZ + dTT) };

        ExtendedPoint { X, Y, Z, T }
    }

    // XXX: See comment on addition, the formula is unified, so this will do for now
    //https://iacr.org/archive/asiacrypt2008/53500329/53500329.pdf (3.1)
    pub fn double(&self) -> Self {
        self.add(&self)
    }

    pub(crate) fn is_on_curve(&self) -> bool {
        let XY = self.X * self.Y;
        let ZT = self.Z * self.T;

        // Y^2 + X^2 == Z^2 - T^2 * D

        let YY = self.Y.square();
        let XX = self.X.square();
        let ZZ = self.Z.square();
        let TT = self.T.square();
        let lhs = YY + XX;
        let rhs = ZZ + TT * FieldElement::EDWARDS_D;

        (XY == ZT) && (lhs == rhs)
    }

    pub fn to_affine(&self) -> AffinePoint {
        let INV_Z = self.Z.invert();

        let x = self.X * INV_Z;
        let y = self.Y * INV_Z;

        AffinePoint { x, y }
    }

    /// Edwards_Isogeny is derived from the doubling formula
    /// XXX: There is a duplicate method in the twisted edwards module to compute the dual isogeny
    /// XXX: Not much point trying to make it generic I think. So what we can do is optimise each respective isogeny method for a=1 or a = -1 (currently, I just made it really slow and simple)
    fn edwards_isogeny(&self, a: FieldElement) -> TwistedExtendedPoint {
        // Convert to affine now, then derive extended version later
        let affine = self.to_affine();
        let x = affine.x;
        let y = affine.y;

        // Compute x
        let xy = x * y;
        let x_numerator = xy + xy;
        let x_denom = y.square() - (a * x.square());
        let new_x = x_numerator * x_denom.invert();

        // Compute y
        let y_numerator = y.square() + (a * x.square());
        let y_denom = (FieldElement::ONE + FieldElement::ONE) - y.square() - (a * x.square());
        let new_y = y_numerator * y_denom.invert();

        TwistedExtendedPoint {
            X: new_x,
            Y: new_y,
            Z: FieldElement::ONE,
            T: new_x * new_y,
        }
    }

    pub fn to_twisted(&self) -> TwistedExtendedPoint {
        self.edwards_isogeny(FieldElement::ONE)
    }

    pub fn negate(&self) -> Self {
        ExtendedPoint {
            X: -self.X,
            Y: self.Y,
            Z: self.Z,
            T: -self.T,
        }
    }

    pub fn torque(&self) -> Self {
        ExtendedPoint {
            X: -self.X,
            Y: -self.Y,
            Z: self.Z,
            T: self.T,
        }
    }

    /// Determine if this point is “torsion-free”, i.e., is contained in
    /// the prime-order subgroup.
    ///
    /// # Return
    ///
    /// * `true` if `self` has zero torsion component and is in the
    /// prime-order subgroup;
    /// * `false` if `self` has a nonzero torsion component and is not
    /// in the prime-order subgroup.
    pub fn is_torsion_free(&self) -> bool {
        (self * BASEPOINT_ORDER) == Self::IDENTITY
    }

    /// Hash using the default domain separation tag and hash function
    pub fn hash_with_defaults(msg: &[u8]) -> Self {
        Self::hash::<ExpandMsgXof<sha3::Shake256>>(msg, DEFAULT_HASH_TO_CURVE_SUITE)
    }

    /// Implements hash to curve according
    /// see <https://datatracker.ietf.org/doc/rfc9380/>
    pub fn hash<X>(msg: &[u8], dst: &[u8]) -> Self
    where
        X: for<'a> ExpandMsg<'a>,
    {
        let mut random_bytes = GenericArray::<u8, U84>::default();
        let dst = [dst];
        let mut expander = X::expand_message(&[msg], &dst, random_bytes.len() * 2).unwrap();
        expander.fill_bytes(&mut random_bytes);
        let u0 = FieldElement::from_okm(&random_bytes);
        expander.fill_bytes(&mut random_bytes);
        let u1 = FieldElement::from_okm(&random_bytes);
        let mut q0 = map_to_curve_elligator2(&u0);
        let mut q1 = map_to_curve_elligator2(&u1);
        q0 = iso448(&q0);
        q1 = iso448(&q1);
        let cofactor = Scalar::from(4u8);

        (q0.to_extended() + q1.to_extended()).scalar_mul(&cofactor)
    }

    /// Encode using the default domain separation tag and hash function
    pub fn encode_with_defaults(msg: &[u8]) -> Self {
        Self::encode::<ExpandMsgXof<sha3::Shake256>>(msg, DEFAULT_ENCODE_TO_CURVE_SUITE)
    }

    /// Implements encode to curve according
    /// see <https://datatracker.ietf.org/doc/rfc9380/>
    pub fn encode<X>(msg: &[u8], dst: &[u8]) -> Self
    where
        X: for<'a> ExpandMsg<'a>,
    {
        let mut random_bytes = GenericArray::<u8, U84>::default();
        let dst = [dst];
        let mut expander = X::expand_message(&[msg], &dst, random_bytes.len()).unwrap();
        expander.fill_bytes(&mut random_bytes);
        let u0 = FieldElement::from_okm(&random_bytes);
        let mut q0 = map_to_curve_elligator2(&u0);
        q0 = iso448(&q0);
        let cofactor = Scalar::from(4u8);

        q0.to_extended().scalar_mul(&cofactor)
    }
}

// ------------------------------------------------------------------------
// Addition and Subtraction
// ------------------------------------------------------------------------

impl<'a, 'b> Add<&'b ExtendedPoint> for &'a ExtendedPoint {
    type Output = ExtendedPoint;
    fn add(self, other: &'b ExtendedPoint) -> ExtendedPoint {
        self.add(other)
    }
}

define_add_variants!(
    LHS = ExtendedPoint,
    RHS = ExtendedPoint,
    Output = ExtendedPoint
);

impl<'b> AddAssign<&'b ExtendedPoint> for ExtendedPoint {
    fn add_assign(&mut self, _rhs: &'b ExtendedPoint) {
        *self = (self as &ExtendedPoint) + _rhs;
    }
}

define_add_assign_variants!(LHS = ExtendedPoint, RHS = ExtendedPoint);

impl<'a, 'b> Sub<&'b ExtendedPoint> for &'a ExtendedPoint {
    type Output = ExtendedPoint;
    fn sub(self, other: &'b ExtendedPoint) -> ExtendedPoint {
        self.add(&other.negate())
    }
}

define_sub_variants!(
    LHS = ExtendedPoint,
    RHS = ExtendedPoint,
    Output = ExtendedPoint
);

impl<'b> SubAssign<&'b ExtendedPoint> for ExtendedPoint {
    fn sub_assign(&mut self, _rhs: &'b ExtendedPoint) {
        *self = (self as &ExtendedPoint) - _rhs;
    }
}

define_sub_assign_variants!(LHS = ExtendedPoint, RHS = ExtendedPoint);

impl<T> Sum<T> for ExtendedPoint
where
    T: Borrow<ExtendedPoint>,
{
    fn sum<I>(iter: I) -> Self
    where
        I: Iterator<Item = T>,
    {
        iter.fold(Self::IDENTITY, |acc, item| acc + item.borrow())
    }
}

// ------------------------------------------------------------------------
// Negation
// ------------------------------------------------------------------------

impl<'a> Neg for &'a ExtendedPoint {
    type Output = ExtendedPoint;

    fn neg(self) -> ExtendedPoint {
        self.negate()
    }
}

impl Neg for ExtendedPoint {
    type Output = ExtendedPoint;

    fn neg(self) -> ExtendedPoint {
        -&self
    }
}

// ------------------------------------------------------------------------
// Scalar multiplication
// ------------------------------------------------------------------------

impl<'b> MulAssign<&'b Scalar> for ExtendedPoint {
    fn mul_assign(&mut self, scalar: &'b Scalar) {
        let result = (self as &ExtendedPoint) * scalar;
        *self = result;
    }
}

define_mul_assign_variants!(LHS = ExtendedPoint, RHS = Scalar);

define_mul_variants!(LHS = ExtendedPoint, RHS = Scalar, Output = ExtendedPoint);
define_mul_variants!(LHS = Scalar, RHS = ExtendedPoint, Output = ExtendedPoint);

impl<'a, 'b> Mul<&'b Scalar> for &'a ExtendedPoint {
    type Output = ExtendedPoint;
    /// Scalar multiplication: compute `scalar * self`.
    fn mul(self, scalar: &'b Scalar) -> ExtendedPoint {
        self.scalar_mul(scalar)
    }
}

impl<'a, 'b> Mul<&'b ExtendedPoint> for &'a Scalar {
    type Output = ExtendedPoint;

    /// Scalar multiplication: compute `scalar * self`.
    fn mul(self, point: &'b ExtendedPoint) -> ExtendedPoint {
        point * self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;

    fn hex_to_field(hex: &'static str) -> FieldElement {
        assert_eq!(hex.len(), 56 * 2);
        let mut bytes = hex_literal::decode(&[hex.as_bytes()]);
        bytes.reverse();
        FieldElement::from_bytes(&bytes)
    }

    #[test]
    fn test_isogeny() {
        let x = hex_to_field("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa955555555555555555555555555555555555555555555555555555555");
        let y = hex_to_field("ae05e9634ad7048db359d6205086c2b0036ed7a035884dd7b7e36d728ad8c4b80d6565833a2a3098bbbcb2bed1cda06bdaeafbcdea9386ed");
        let a = AffinePoint { x, y }.to_extended();
        let twist_a = a.to_twisted().to_untwisted();
        assert!(twist_a == a.double().double())
    }

    // XXX: Move this to constants folder to test all global constants
    #[test]
    fn derive_base_points() {
        use crate::{GOLDILOCKS_BASE_POINT, TWISTED_EDWARDS_BASE_POINT};

        // This was the original basepoint which had order 2q;
        let old_x = hex_to_field("4F1970C66BED0DED221D15A622BF36DA9E146570470F1767EA6DE324A3D3A46412AE1AF72AB66511433B80E18B00938E2626A82BC70CC05E");
        let old_y = hex_to_field("693F46716EB6BC248876203756C9C7624BEA73736CA3984087789C1E05A0C2D73AD3FF1CE67C39C4FDBD132C4ED7C8AD9808795BF230FA14");
        let old_bp = AffinePoint { x: old_x, y: old_y }.to_extended();

        // This is the new basepoint, that is in the ed448 paper
        let new_x = hex_to_field("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa955555555555555555555555555555555555555555555555555555555");
        let new_y = hex_to_field("ae05e9634ad7048db359d6205086c2b0036ed7a035884dd7b7e36d728ad8c4b80d6565833a2a3098bbbcb2bed1cda06bdaeafbcdea9386ed");
        let new_bp = AffinePoint { x: new_x, y: new_y }.to_extended();

        // Doubling the old basepoint, should give us the new basepoint
        assert_eq!(old_bp.double(), new_bp);

        // XXX: Unfortunately, the test vectors in libdecaf currently use the old basepoint.
        // We need to update this. But for now, I use the old basepoint so that I can check against libdecaf

        assert_eq!(GOLDILOCKS_BASE_POINT, old_bp);

        // The Twisted basepoint can be derived by using the isogeny
        assert_eq!(old_bp.to_twisted(), TWISTED_EDWARDS_BASE_POINT)
    }

    #[test]
    fn test_is_on_curve() {
        let x = hex_to_field("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa955555555555555555555555555555555555555555555555555555555");
        let y = hex_to_field("ae05e9634ad7048db359d6205086c2b0036ed7a035884dd7b7e36d728ad8c4b80d6565833a2a3098bbbcb2bed1cda06bdaeafbcdea9386ed");
        let gen = AffinePoint { x, y }.to_extended();
        assert!(gen.is_on_curve());
    }
    #[test]
    fn test_compress_decompress() {
        let x = hex_to_field("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa955555555555555555555555555555555555555555555555555555555");
        let y = hex_to_field("ae05e9634ad7048db359d6205086c2b0036ed7a035884dd7b7e36d728ad8c4b80d6565833a2a3098bbbcb2bed1cda06bdaeafbcdea9386ed");
        let gen = AffinePoint { x, y }.to_extended();

        let decompressed_point = gen.compress().decompress();
        assert!(<Choice as Into<bool>>::into(decompressed_point.is_some()));

        assert!(gen == decompressed_point.unwrap());
    }
    #[test]
    fn test_decompress_compress() {
        let bytes = hex!("649c6a53b109897d962d033f23d01fd4e1053dddf3746d2ddce9bd66aea38ccfc3df061df03ca399eb806312ab3037c0c31523142956ada780");
        let compressed = CompressedEdwardsY(bytes);
        let decompressed = compressed.decompress().unwrap();

        let recompressed = decompressed.compress();

        assert_eq!(bytes, recompressed.0);
    }
    #[test]
    fn test_just_decompress() {
        let bytes = hex!("649c6a53b109897d962d033f23d01fd4e1053dddf3746d2ddce9bd66aea38ccfc3df061df03ca399eb806312ab3037c0c31523142956ada780");
        let compressed = CompressedEdwardsY(bytes);
        let decompressed = compressed.decompress().unwrap();

        assert_eq!(decompressed.X, hex_to_field("39c41cea305d737df00de8223a0d5f4d48c8e098e16e9b4b2f38ac353262e119cb5ff2afd6d02464702d9d01c9921243fc572f9c718e2527"));
        assert_eq!(decompressed.Y, hex_to_field("a7ad5629142315c3c03730ab126380eb99a33cf01d06dfc3cf8ca3ae66bde9dc2d6d74f3dd3d05e1d41fd0233f032d967d8909b1536a9c64"));

        let bytes = hex!("010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000");
        let compressed = CompressedEdwardsY(bytes);
        let decompressed = compressed.decompress().unwrap();

        assert_eq!(decompressed.X, hex_to_field("0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"));
        assert_eq!(decompressed.Y, hex_to_field("0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"));
    }
    #[test]
    fn test_is_torsion_free() {
        assert!(ExtendedPoint::GENERATOR.is_torsion_free());
        assert!(ExtendedPoint::IDENTITY.is_torsion_free());

        let bytes = hex!("13b6714c7a5f53101bbec88f2f17cd30f42e37fae363a5474efb4197ed6005df5861ae178a0c2c16ad378b7befed0d0904b7ced35e9f674180");
        let compressed = CompressedEdwardsY(bytes);
        let decompressed = compressed.decompress().unwrap();
        assert!(!decompressed.is_torsion_free());
    }

    #[test]
    fn hash() {
        const DST: &[u8] = b"QUUX-V01-CS02-with-edwards448_XOF:SHAKE256_ELL2_RO_";
        const MSGS: &[(&[u8], [u8; 56], [u8; 56])] = &[
            (b"", hex!("73036d4a88949c032f01507005c133884e2f0d81f9a950826245dda9e844fc78186c39daaa7147ead3e462cff60e9c6340b58134480b4d17"), hex!("94c1d61b43728e5d784ef4fcb1f38e1075f3aef5e99866911de5a234f1aafdc26b554344742e6ba0420b71b298671bbeb2b7736618634610")),
            (b"abc", hex!("4e0158acacffa545adb818a6ed8e0b870e6abc24dfc1dc45cf9a052e98469275d9ff0c168d6a5ac7ec05b742412ee090581f12aa398f9f8c"), hex!("894d3fa437b2d2e28cdc3bfaade035430f350ec5239b6b406b5501da6f6d6210ff26719cad83b63e97ab26a12df6dec851d6bf38e294af9a")),
            (b"abcdef0123456789", hex!("2c25b4503fadc94b27391933b557abdecc601c13ed51c5de68389484f93dbd6c22e5f962d9babf7a39f39f994312f8ca23344847e1fbf176"), hex!("d5e6f5350f430e53a110f5ac7fcc82a96cb865aeca982029522d32601e41c042a9dfbdfbefa2b0bdcdc3bc58cca8a7cd546803083d3a8548")),
            (b"q128_qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq", hex!("a1861a9464ae31249a0e60bf38791f3663049a3f5378998499a83292e159a2fecff838eb9bc6939e5c6ae76eb074ad4aae39b55b72ca0b9a"), hex!("580a2798c5b904f8adfec5bd29fb49b4633cd9f8c2935eb4a0f12e5dfa0285680880296bb729c6405337525fb5ed3dff930c137314f60401")),
            (b"a512_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", hex!("987c5ac19dd4b47835466a50b2d9feba7c8491b8885a04edf577e15a9f2c98b203ec2cd3e5390b3d20bba0fa6fc3eecefb5029a317234401"), hex!("5e273fcfff6b007bb6771e90509275a71ff1480c459ded26fc7b10664db0a68aaa98bc7ecb07e49cf05b80ae5ac653fbdd14276bbd35ccbc")),
        ];

        for (msg, x, y) in MSGS {
            let p = ExtendedPoint::hash::<ExpandMsgXof<sha3::Shake256>>(msg, DST);
            assert!(p.is_on_curve());
            let p = p.to_affine();
            let mut xx = [0u8; 56];
            xx.copy_from_slice(&x[..]);
            xx.reverse();
            let mut yy = [0u8; 56];
            yy.copy_from_slice(&y[..]);
            yy.reverse();
            assert_eq!(p.x.to_bytes(), xx);
            assert_eq!(p.y.to_bytes(), yy);
        }
    }

    #[test]
    fn encode() {
        const DST: &[u8] = b"QUUX-V01-CS02-with-edwards448_XOF:SHAKE256_ELL2_NU_";
        const MSGS: &[(&[u8], [u8; 56], [u8; 56])] = &[
            (b"", hex!("eb5a1fc376fd73230af2de0f3374087cc7f279f0460114cf0a6c12d6d044c16de34ec2350c34b26bf110377655ab77936869d085406af71e"), hex!("df5dcea6d42e8f494b279a500d09e895d26ac703d75ca6d118e8ca58bf6f608a2a383f292fce1563ff995dce75aede1fdc8e7c0c737ae9ad")),
            (b"abc", hex!("4623a64bceaba3202df76cd8b6e3daf70164f3fcbda6d6e340f7fab5cdf89140d955f722524f5fe4d968fef6ba2853ff4ea086c2f67d8110"), hex!("abaac321a169761a8802ab5b5d10061fec1a83c670ac6bc95954700317ee5f82870120e0e2c5a21b12a0c7ad17ebd343363604c4bcecafd1")),
            (b"abcdef0123456789", hex!("e9eb562e76db093baa43a31b7edd04ec4aadcef3389a7b9c58a19cf87f8ae3d154e134b6b3ed45847a741e33df51903da681629a4b8bcc2e"), hex!("0cf6606927ad7eb15dbc193993bc7e4dda744b311a8ec4274c8f738f74f605934582474c79260f60280fe35bd37d4347e59184cbfa12cbc4")),
            (b"q128_qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq", hex!("122a3234d34b26c69749f23356452bf9501efa2d94859d5ef741fef024156d9d191a03a2ad24c38186f93e02d05572575968b083d8a39738"), hex!("ddf55e74eb4414c2c1fa4aa6bc37c4ab470a3fed6bb5af1e43570309b162fb61879bb15f9ea49c712efd42d0a71666430f9f0d4a20505050")),
            (b"a512_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", hex!("221704949b1ce1ab8dd174dc9b8c56fcffa27179569ce9219c0c2fe183d3d23343a4c42a0e2e9d6b9d0feb1df3883ec489b6671d1fa64089"), hex!("ebdecfdc87142d1a919034bf22ecfad934c9a85effff14b594ae2c00943ca62a39d6ee3be9df0bb504ce8a9e1669bc6959c42ad6a1d3b686")),
        ];

        for (msg, x, y) in MSGS {
            let p = ExtendedPoint::encode::<ExpandMsgXof<sha3::Shake256>>(msg, DST);
            assert!(p.is_on_curve());
            let p = p.to_affine();
            let mut xx = [0u8; 56];
            xx.copy_from_slice(&x[..]);
            xx.reverse();
            let mut yy = [0u8; 56];
            yy.copy_from_slice(&y[..]);
            yy.reverse();
            assert_eq!(p.x.to_bytes(), xx);
            assert_eq!(p.y.to_bytes(), yy);
        }
    }
}
