use crate::circuit::ecc::{general_ecc::GeneralEccChip, AssignedPoint, EccConfig};
use crate::circuit::integer::IntegerInstructions;
use crate::circuit::AssignedInteger;
use crate::rns::Integer;
use halo2::arithmetic::{CurveAffine, FieldExt};
use halo2::circuit::Region;
use halo2::plonk::Error;
use halo2arith::halo2;
use halo2arith::main_gate::five::main_gate::MainGateConfig;
use halo2arith::main_gate::five::range::RangeConfig;

use super::integer::{IntegerChip, IntegerConfig};

#[derive(Clone, Debug)]
pub struct EcdsaConfig {
    main_gate_config: MainGateConfig,
    range_config: RangeConfig,
}

impl EcdsaConfig {
    pub fn new(range_config: RangeConfig, main_gate_config: MainGateConfig) -> Self {
        Self {
            range_config,
            main_gate_config,
        }
    }

    pub fn ecc_chip_config(&self) -> EccConfig {
        EccConfig::new(self.range_config.clone(), self.main_gate_config.clone())
    }

    fn integer_chip_config(&self) -> IntegerConfig {
        IntegerConfig::new(self.range_config.clone(), self.main_gate_config.clone())
    }
}

#[derive(Clone, Debug)]
pub struct EcdsaSig<'a, W: FieldExt, N: FieldExt> {
    pub r: Integer<'a, W, N>,
    pub s: Integer<'a, W, N>,
}

pub struct AssignedEcdsaSig<N: FieldExt> {
    pub r: AssignedInteger<N>,
    pub s: AssignedInteger<N>,
}

pub struct AssignedPublicKey<N: FieldExt> {
    pub point: AssignedPoint<N>,
}

pub struct EcdsaChip<E: CurveAffine, N: FieldExt>(GeneralEccChip<E, N>);

impl<E: CurveAffine, N: FieldExt> EcdsaChip<E, N> {
    pub fn new(ecc_chip: GeneralEccChip<E, N>) -> Self {
        Self(ecc_chip)
    }

    pub fn scalar_field_chip(&self) -> IntegerChip<E::ScalarExt, N> {
        self.0.scalar_field_chip()
    }

    fn ecc_chip(&self) -> GeneralEccChip<E, N> {
        self.0.clone()
    }
}

impl<E: CurveAffine, N: FieldExt> EcdsaChip<E, N> {
    // https://en.wikipedia.org/wiki/Elliptic_Curve_Digital_Signature_Algorithm
    fn verify(
        &self,
        region: &mut Region<'_, N>,
        sig: &AssignedEcdsaSig<N>,
        pk: &AssignedPublicKey<N>,
        msg_hash: &AssignedInteger<N>,
        offset: &mut usize,
    ) -> Result<(), Error> {
        let ecc_chip = self.ecc_chip();
        let scalar_chip = ecc_chip.scalar_field_chip();
        let base_chip = ecc_chip.base_field_chip();

        // 1. check 0 < r, s < n

        // // since `assert_not_zero` already includes a in-field check, we can just call `assert_not_zero`
        scalar_chip.assert_not_zero(region, &sig.r, offset)?;
        scalar_chip.assert_not_zero(region, &sig.s, offset)?;

        // 2. w = s^(-1) (mod n)
        let (s_inv, _) = scalar_chip.invert(region, &sig.s, offset)?;

        // 3. u1 = m' * w (mod n)
        let u1 = scalar_chip.mul(region, &msg_hash, &s_inv, offset)?;

        // 4. u2 = r * w (mod n)
        let u2 = scalar_chip.mul(region, &sig.r, &s_inv, offset)?;

        // 5. compute Q = u1*G + u2*pk
        let e_gen = ecc_chip.assign_point(region, Some(E::generator()), offset)?;
        let g1 = ecc_chip.mul(region, &e_gen, &u1, 2, offset)?;
        let g2 = ecc_chip.mul(region, &pk.point, &u2, 2, offset)?;
        let q = ecc_chip.add(region, &g1, &g2, offset)?;

        // 6. reduce q_x in E::ScalarExt
        // assuming E::Base/E::ScalarExt have the same number of limbs
        let q_x = q.get_x();
        let q_x_reduced_in_q = base_chip.reduce(region, &q_x, offset)?;
        let q_x_reduced_in_r = scalar_chip.reduce(region, &q_x_reduced_in_q, offset)?;

        // 7. check if Q.x == r (mod n)
        scalar_chip.assert_strict_equal(region, &q_x_reduced_in_r, &sig.r, offset)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use crate::circuit::ecc::general_ecc::GeneralEccChip;
    use crate::circuit::ecc::EccConfig;
    use crate::circuit::ecdsa::{AssignedEcdsaSig, AssignedPublicKey, EcdsaChip, EcdsaConfig};
    use crate::circuit::integer::{IntegerConfig, IntegerInstructions};
    use crate::NUMBER_OF_LOOKUP_LIMBS;
    use group::ff::Field;

    #[cfg(not(feature = "kzg"))]
    use group::ff::PrimeField;

    use group::{prime::PrimeCurveAffine, Curve};
    use halo2::arithmetic::CurveAffine;
    use halo2::arithmetic::FieldExt;
    use halo2::circuit::{Layouter, SimpleFloorPlanner};
    use halo2::dev::MockProver;
    use halo2::plonk::{Circuit, ConstraintSystem, Error};
    use halo2arith::halo2;
    use halo2arith::main_gate::five::main_gate::{MainGate, MainGateConfig};
    use halo2arith::main_gate::five::range::RangeInstructions;
    use halo2arith::main_gate::five::range::{RangeChip, RangeConfig};
    use rand::thread_rng;

    #[cfg(feature = "kzg")]
    use crate::halo2::arithmetic::BaseExt;

    const BIT_LEN_LIMB: usize = 68;

    #[derive(Clone, Debug)]
    struct TestCircuitEcdsaVerifyConfig {
        main_gate_config: MainGateConfig,
        range_config: RangeConfig,
    }

    impl TestCircuitEcdsaVerifyConfig {
        fn new<C: CurveAffine, N: FieldExt>(meta: &mut ConstraintSystem<N>) -> Self {
            let (rns_base, rns_scalar) = GeneralEccChip::<C, N>::rns(BIT_LEN_LIMB);
            let main_gate_config = MainGate::<N>::configure(meta);
            let mut overflow_bit_lengths: Vec<usize> = vec![];
            overflow_bit_lengths.extend(rns_base.overflow_lengths());
            overflow_bit_lengths.extend(rns_scalar.overflow_lengths());
            let range_config = RangeChip::<N>::configure(meta, &main_gate_config, overflow_bit_lengths);
            TestCircuitEcdsaVerifyConfig {
                main_gate_config,
                range_config,
            }
        }

        fn ecc_chip_config(&self) -> EccConfig {
            EccConfig::new(self.range_config.clone(), self.main_gate_config.clone())
        }

        fn ecdsa_chip_config(&self) -> EcdsaConfig {
            EcdsaConfig::new(self.range_config.clone(), self.main_gate_config.clone())
        }

        fn integer_chip_config(&self) -> IntegerConfig {
            IntegerConfig::new(self.range_config.clone(), self.main_gate_config.clone())
        }

        fn config_range<N: FieldExt>(&self, layouter: &mut impl Layouter<N>) -> Result<(), Error> {
            let bit_len_lookup = BIT_LEN_LIMB / NUMBER_OF_LOOKUP_LIMBS;
            let range_chip = RangeChip::<N>::new(self.range_config.clone(), bit_len_lookup);
            range_chip.load_limb_range_table(layouter)?;
            range_chip.load_overflow_range_tables(layouter)?;

            Ok(())
        }
    }

    #[derive(Default, Clone)]
    struct TestCircuitEcdsaVerify<E: CurveAffine, N: FieldExt> {
        aux_generator: E,
        window_size: usize,
        _marker: PhantomData<N>,
    }

    impl<E: CurveAffine, N: FieldExt> Circuit<N> for TestCircuitEcdsaVerify<E, N> {
        type Config = TestCircuitEcdsaVerifyConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<N>) -> Self::Config {
            TestCircuitEcdsaVerifyConfig::new::<E, N>(meta)
        }

        fn synthesize(&self, config: Self::Config, mut layouter: impl Layouter<N>) -> Result<(), Error> {
            // let mut ecdsa_chip = EcdsaChip::<E, N>::new(config.ecdsa_chip_config(), self.window_size, BIT_LEN_LIMB)?;
            let ecc_chip_config = config.ecc_chip_config();
            let mut ecc_chip = GeneralEccChip::<E, N>::new(ecc_chip_config, BIT_LEN_LIMB);
            let scalar_chip = ecc_chip.scalar_field_chip();

            let mut rng = thread_rng();

            // generate a valid signature
            let generator = <E as PrimeCurveAffine>::generator();
            let sk = <E as CurveAffine>::ScalarExt::random(&mut rng);
            let pk = generator * sk;
            let pk = pk.to_affine();

            let m_hash = <E as CurveAffine>::ScalarExt::random(&mut rng);
            let randomness = <E as CurveAffine>::ScalarExt::random(&mut rng);
            let randomness_inv = randomness.invert().unwrap();
            let sig_point = generator * randomness;
            let x = sig_point.to_affine().coordinates().unwrap().x().clone();
            println!("E char = {}", E::ScalarExt::MODULUS);
            println!("x coord = {:?}", x);

            cfg_if::cfg_if! {
                if #[cfg(feature = "kzg")] {
                    let x_repr = &mut Vec::with_capacity(32);
                    x.write(x_repr)?;
                } else {
                    let mut x_repr = [0u8; 32];
                    x_repr.copy_from_slice(x.to_repr().as_ref());
                }
            }

            let mut x_bytes = [0u8; 64];
            x_bytes[..32].copy_from_slice(&x_repr[..]);

            let x_bytes_on_n = <E as CurveAffine>::ScalarExt::from_bytes_wide(&x_bytes); // get x cordinate (E::Base) on E::Scalar
            let sig_s = randomness_inv * (m_hash + x_bytes_on_n * sk);
            println!("sig.r on Emulated = {:?}", x_bytes_on_n.clone());

            // verify with Emulated
            {
                let s_inv = sig_s.invert().unwrap();
                let u1 = m_hash * s_inv;
                let u2 = x_bytes_on_n * s_inv;
                let g1 = E::generator().mul(u1);
                let g2 = pk.mul(u2);
                let q = g1 + g2;
                let q = q.to_affine();
                println!("q on Emulated = {:?}", q);
            }

            let rns_scalar = ecc_chip.rns_scalar();

            let integer_r = rns_scalar.new(x_bytes_on_n);
            let integer_s = rns_scalar.new(sig_s);
            let integer_m_hash = rns_scalar.new(m_hash);
            let msg_hash = integer_m_hash.clone();

            layouter.assign_region(
                || "assign aux values",
                |mut region| {
                    let offset = &mut 0;
                    ecc_chip.assign_aux_generator(&mut region, Some(self.aux_generator), offset)?;
                    ecc_chip.assign_aux(&mut region, self.window_size, 1, offset)?;
                    Ok(())
                },
            )?;

            let ecdsa_chip = EcdsaChip::new(ecc_chip.clone());

            layouter.assign_region(
                || "region 0",
                |mut region| {
                    let offset = &mut 0;

                    let r_assigned = scalar_chip.assign_integer(&mut region, integer_r.clone().into(), offset)?;
                    let s_assigned = scalar_chip.assign_integer(&mut region, integer_s.clone().into(), offset)?;
                    let sig = AssignedEcdsaSig { r: r_assigned, s: s_assigned };

                    let pk_in_circuit = ecc_chip.assign_point(&mut region, Some(pk.into()), offset)?;
                    let pk_assigned = AssignedPublicKey { point: pk_in_circuit };
                    let msg_hash = scalar_chip.assign_integer(&mut region, msg_hash.clone().into(), offset)?;
                    ecdsa_chip.verify(&mut region, &sig, &pk_assigned, &msg_hash, offset)
                },
            )?;

            config.config_range(&mut layouter)?;

            Ok(())
        }
    }

    #[test]
    #[cfg(feature = "zcash")]
    fn test_ecdsa_verifier() {
        use group::Group;
        // use halo2::pasta::Eq as Projective;
        // use halo2::pasta::EqAffine as E;
        // use halo2::pasta::Fp as N;

        cfg_if::cfg_if! {
            if #[cfg(feature = "kzg")] {
                use halo2::pairing::bn256::Fq as Field;
                use halo2::pairing::bn256::G1Affine as Curve;
                use halo2::pairing::bn256::G1 as CurveProjective;
            } else {
                use halo2::pasta::EqAffine as Curve;
                use halo2::pasta::Eq as CurveProjective;
                use halo2::pasta::Fp as Field;
            }
        }

        let k = 20;

        let mut rng = thread_rng();
        let aux_generator = CurveProjective::random(&mut rng).to_affine();

        // testcase: normal
        let circuit = TestCircuitEcdsaVerify::<Curve, Field> {
            aux_generator,
            window_size: 2,
            _marker: PhantomData,
        };

        let public_inputs = vec![vec![]];
        let prover = match MockProver::run(k, &circuit, public_inputs) {
            Ok(prover) => prover,
            Err(e) => panic!("{:#?}", e),
        };

        assert_eq!(prover.verify(), Ok(()));
    }
}
