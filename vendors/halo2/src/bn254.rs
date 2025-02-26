use std::{
    io::{self, Write},
    marker::PhantomData,
    ops::Deref,
};

use ff::PrimeField;
use halo2_proofs::{
    plonk::{sealed, Column, Fixed},
    poly::{Coeff, LagrangeCoeff, Polynomial},
    transcript::{
        Challenge255, EncodedChallenge, Transcript, TranscriptWrite, TranscriptWriterBuffer,
    },
};
use halo2curves::{Coordinates, CurveAffine};

use tachyon_rs::math::elliptic_curves::bn::bn254::{
    Fr as FrImpl, G1JacobianPoint as G1JacobianPointImpl, G1Point2 as G1Point2Impl,
};

pub struct G1MSM;
pub struct G1MSMGpu;
pub struct G1JacobianPoint(pub G1JacobianPointImpl);
pub struct G1Point2(pub G1Point2Impl);
pub struct Fr(pub FrImpl);
pub struct InstanceSingle {
    pub instance_values: Vec<Vec<Fr>>,
    pub instance_polys: Vec<Vec<Fr>>,
}
pub struct AdviceSingle {
    pub advice_polys: Vec<Vec<Fr>>,
    pub advice_blinds: Vec<Fr>,
}

#[cxx::bridge(namespace = "tachyon::halo2_api::bn254")]
pub mod ffi {
    extern "Rust" {
        type G1MSM;
        type G1MSMGpu;
        type G1JacobianPoint;
        type G1Point2;
        type Fr;
        type InstanceSingle;
        type AdviceSingle;
    }

    unsafe extern "C++" {
        include!("vendors/halo2/include/bn254_msm.h");
        #[cfg(feature = "gpu")]
        include!("vendors/halo2/include/bn254_msm_gpu.h");

        fn create_g1_msm(degree: u8) -> Box<G1MSM>;
        fn destroy_g1_msm(msm: Box<G1MSM>);
        unsafe fn g1_point2_msm(
            msm: *mut G1MSM,
            bases: &[G1Point2],
            scalars: &[Fr],
        ) -> Box<G1JacobianPoint>;
        #[cfg(feature = "gpu")]
        fn create_g1_msm_gpu(degree: u8, algorithm: i32) -> Box<G1MSMGpu>;
        #[cfg(feature = "gpu")]
        fn destroy_g1_msm_gpu(msm: Box<G1MSMGpu>);
        #[cfg(feature = "gpu")]
        unsafe fn g1_point2_msm_gpu(
            msm: *mut G1MSMGpu,
            bases: &[G1Point2],
            scalars: &[Fr],
        ) -> Box<G1JacobianPoint>;
    }

    unsafe extern "C++" {
        include!("vendors/halo2/include/bn254_blake2b_writer.h");

        type Blake2bWriter;

        fn new_blake2b_writer() -> UniquePtr<Blake2bWriter>;
        fn update(self: Pin<&mut Blake2bWriter>, data: &[u8]);
        fn finalize(self: Pin<&mut Blake2bWriter>, result: &mut [u8; 64]);
        fn state(&self) -> Vec<u8>;
    }

    unsafe extern "C++" {
        include!("vendors/halo2/include/bn254_shplonk_proving_key.h");

        type SHPlonkProvingKey;

        fn new_proving_key(data: &[u8]) -> UniquePtr<SHPlonkProvingKey>;
        fn advice_column_phases(&self) -> &[u8];
        fn blinding_factors(&self) -> usize;
        fn challenge_phases(&self) -> &[u8];
        fn constants(&self) -> Vec<usize>;
        fn num_advice_columns(&self) -> usize;
        fn num_challenges(&self) -> usize;
        fn num_instance_columns(&self) -> usize;
        fn phases(&self) -> Vec<u8>;
        fn transcript_repr(self: Pin<&mut SHPlonkProvingKey>, prover: &SHPlonkProver) -> Box<Fr>;
    }

    unsafe extern "C++" {
        include!("vendors/halo2/include/bn254_shplonk_prover.h");

        type SHPlonkProver;

        fn new_shplonk_prover(k: u32, s: &Fr) -> UniquePtr<SHPlonkProver>;
        fn k(&self) -> u32;
        fn n(&self) -> u64;
        fn commit(&self, scalars: &[Fr]) -> Box<G1JacobianPoint>;
        fn commit_lagrange(&self, scalars: &[Fr]) -> Box<G1JacobianPoint>;
        fn set_rng(self: Pin<&mut SHPlonkProver>, state: &[u8]);
        fn set_transcript(self: Pin<&mut SHPlonkProver>, state: &[u8]);
        fn set_extended_domain(self: Pin<&mut SHPlonkProver>, pk: &SHPlonkProvingKey);
        // TODO(chokobole): Needs to take `instance_singles` and `advice_singles` as a slice.
        fn create_proof(
            self: Pin<&mut SHPlonkProver>,
            key: &SHPlonkProvingKey,
            instance_singles: Vec<InstanceSingle>,
            advice_singles: Vec<AdviceSingle>,
            challenges: Vec<Fr>,
        );
        fn finalize_transcript(self: Pin<&mut SHPlonkProver>) -> Vec<u8>;
    }
}

pub struct Blake2bWrite<W: Write, C: CurveAffine, E: EncodedChallenge<C>> {
    state: cxx::UniquePtr<ffi::Blake2bWriter>,
    writer: W,
    _marker: PhantomData<(W, C, E)>,
}

impl<W: Write, C: CurveAffine> Blake2bWrite<W, C, Challenge255<C>> {
    pub fn state(&self) -> Vec<u8> {
        self.state.state()
    }
}

impl<W: Write, C: CurveAffine> Transcript<C, Challenge255<C>>
    for Blake2bWrite<W, C, Challenge255<C>>
{
    fn squeeze_challenge(&mut self) -> Challenge255<C> {
        // Prefix to a prover's message soliciting a challenge
        const BLAKE2B_PREFIX_CHALLENGE: u8 = 0;
        self.state.pin_mut().update(&[BLAKE2B_PREFIX_CHALLENGE]);
        let mut result: [u8; 64] = [0; 64];
        self.state.pin_mut().finalize(&mut result);
        Challenge255::<C>::new(&result)
    }

    fn common_point(&mut self, point: C) -> io::Result<()> {
        // Prefix to a prover's message containing a curve point
        const BLAKE2B_PREFIX_POINT: u8 = 1;
        self.state.pin_mut().update(&[BLAKE2B_PREFIX_POINT]);
        let coords: Coordinates<C> = Option::from(point.coordinates()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "cannot write points at infinity to the transcript",
            )
        })?;
        self.state.pin_mut().update(coords.x().to_repr().as_ref());
        self.state.pin_mut().update(coords.y().to_repr().as_ref());

        Ok(())
    }

    fn common_scalar(&mut self, scalar: C::Scalar) -> io::Result<()> {
        // Prefix to a prover's message containing a scalar
        const BLAKE2B_PREFIX_SCALAR: u8 = 2;
        self.state.pin_mut().update(&[BLAKE2B_PREFIX_SCALAR]);
        self.state.pin_mut().update(scalar.to_repr().as_ref());
        Ok(())
    }
}

impl<W: Write, C: CurveAffine> TranscriptWrite<C, Challenge255<C>>
    for Blake2bWrite<W, C, Challenge255<C>>
{
    fn write_point(&mut self, point: C) -> io::Result<()> {
        self.common_point(point)?;
        let compressed = point.to_bytes();
        self.writer.write_all(compressed.as_ref())
    }

    fn write_scalar(&mut self, scalar: C::Scalar) -> io::Result<()> {
        self.common_scalar(scalar)?;
        let data = scalar.to_repr();
        self.writer.write_all(data.as_ref())
    }
}

impl<W: Write, C: CurveAffine> TranscriptWriterBuffer<W, C, Challenge255<C>>
    for Blake2bWrite<W, C, Challenge255<C>>
{
    /// Initialize a transcript given an output buffer.
    fn init(writer: W) -> Self {
        Blake2bWrite {
            state: ffi::new_blake2b_writer(),
            writer: writer,
            _marker: PhantomData,
        }
    }

    fn finalize(self) -> W {
        // TODO: handle outstanding scalars? see issue #138
        self.writer
    }
}

pub struct SHPlonkProvingKey {
    inner: cxx::UniquePtr<ffi::SHPlonkProvingKey>,
}

impl Deref for SHPlonkProvingKey {
    type Target = ffi::SHPlonkProvingKey;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl SHPlonkProvingKey {
    pub fn from(data: &[u8]) -> SHPlonkProvingKey {
        SHPlonkProvingKey {
            inner: ffi::new_proving_key(data),
        }
    }

    pub fn inner(&self) -> &ffi::SHPlonkProvingKey {
        &self.inner
    }

    // NOTE(chokobole): We name this as plural since it contains multi phases.
    // pk.vk.cs.advice_column_phase
    pub fn advice_column_phases(&self) -> &[sealed::Phase] {
        unsafe {
            let phases: &[sealed::Phase] = std::mem::transmute(self.inner.advice_column_phases());
            phases
        }
    }

    // pk.vk.cs.blinding_factors()
    pub fn blinding_factors(&self) -> usize {
        self.inner.blinding_factors()
    }

    // NOTE(chokobole): We name this as plural since it contains multi phases.
    // pk.vk.cs.challenge_phase
    pub fn challenge_phases(&self) -> &[sealed::Phase] {
        unsafe {
            let phases: &[sealed::Phase] = std::mem::transmute(self.inner.challenge_phases());
            phases
        }
    }

    // pk.vk.cs.constants
    pub fn constants(&self) -> Vec<Column<Fixed>> {
        let constants = self
            .inner
            .constants()
            .iter()
            .map(|index| Column {
                index: *index,
                column_type: Fixed,
            })
            .collect::<Vec<_>>();
        constants
    }

    // pk.vk.cs.num_advice_columns
    pub fn num_advice_columns(&self) -> usize {
        self.inner.num_advice_columns()
    }

    // pk.vk.cs.num_challenges
    pub fn num_challenges(&self) -> usize {
        self.inner.num_challenges()
    }

    // pk.vk.cs.num_instance_columns
    pub fn num_instance_columns(&self) -> usize {
        self.inner.num_instance_columns()
    }

    // pk.vk.cs.phases()
    pub fn phases(&self) -> Vec<sealed::Phase> {
        unsafe {
            let phases: Vec<sealed::Phase> = std::mem::transmute(self.inner.phases());
            phases
        }
    }

    // pk.vk.transcript_repr
    pub fn transcript_repr(&mut self, prover: &SHPlonkProver) -> halo2curves::bn256::Fr {
        *unsafe {
            std::mem::transmute::<_, Box<halo2curves::bn256::Fr>>(
                self.inner.pin_mut().transcript_repr(prover.inner()),
            )
        }
    }
}

pub struct SHPlonkProver {
    inner: cxx::UniquePtr<ffi::SHPlonkProver>,
}

impl SHPlonkProver {
    pub fn new(k: u32, s: &halo2curves::bn256::Fr) -> SHPlonkProver {
        let cpp_s = unsafe { std::mem::transmute::<_, &Fr>(s) };
        SHPlonkProver {
            inner: ffi::new_shplonk_prover(k, cpp_s),
        }
    }

    pub(crate) fn inner(&self) -> &ffi::SHPlonkProver {
        &self.inner
    }

    pub fn k(&self) -> u32 {
        self.inner.k()
    }

    pub fn n(&self) -> u64 {
        self.inner.n()
    }

    pub fn commit(
        &self,
        poly: &Polynomial<halo2curves::bn256::Fr, Coeff>,
    ) -> halo2curves::bn256::G1 {
        *unsafe {
            let scalars: &[Fr] = std::mem::transmute(poly.deref());
            std::mem::transmute::<_, Box<halo2curves::bn256::G1>>(self.inner.commit(scalars))
        }
    }

    pub fn commit_lagrange(
        &self,
        poly: &Polynomial<halo2curves::bn256::Fr, LagrangeCoeff>,
    ) -> halo2curves::bn256::G1 {
        *unsafe {
            let scalars: &[Fr] = std::mem::transmute(poly.deref());
            std::mem::transmute::<_, Box<halo2curves::bn256::G1>>(
                self.inner.commit_lagrange(scalars),
            )
        }
    }

    pub fn set_rng(&mut self, state: &[u8]) {
        self.inner.pin_mut().set_rng(state)
    }

    pub fn set_transcript(&mut self, state: &[u8]) {
        self.inner.pin_mut().set_transcript(state)
    }

    pub fn set_extended_domain(&mut self, pk: &SHPlonkProvingKey) {
        self.inner.pin_mut().set_extended_domain(pk.inner())
    }

    pub fn create_proof(
        &mut self,
        key: &SHPlonkProvingKey,
        instance_singles: Vec<InstanceSingle>,
        advice_singles: Vec<AdviceSingle>,
        challenges: Vec<Fr>,
    ) {
        self.inner
            .pin_mut()
            .create_proof(key, instance_singles, advice_singles, challenges)
    }

    pub fn finalize_transcript(&mut self) -> Vec<u8> {
        self.inner.pin_mut().finalize_transcript()
    }
}
