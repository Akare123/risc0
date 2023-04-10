// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Run the zkVM guest and prove its results.
//!
//! # Usage
//! The primary use of this module is to provably run a zkVM guest by use of a
//! [Prover]. See the [Prover] documentation for more detailed usage
//! information.
//!
//! ```ignore
//! // In real code, the ELF & Image ID would be generated by `risc0-build` scripts for the guest code
//! use methods::{EXAMPLE_ELF, EXAMPLE_ID};
//! use risc0_zkvm::Prover;
//!
//! let input = to_vec(&input);
//! let mut prover = Prover::new_with_opts(&EXAMPLE_ELF, EXAMPLE_ID,
//!                                        ProverOpts::defaults().with_stdin(input))?;
//! let receipt = prover.run()?;
//! ```

mod exec;
pub mod io;
pub(crate) mod loader;
mod plonk;
// Preflight is a work in progress right now; don't include it in documentation
// yet.
#[doc(hidden)]
pub mod preflight;
#[cfg(feature = "profiler")]
pub mod profiler;

use std::{
    cell::RefCell,
    cmp::min,
    collections::HashMap,
    fmt::Debug,
    io::{stderr, stdin, stdout, BufRead, BufReader, Cursor, Read, Write},
    mem::take,
    rc::Rc,
    str::from_utf8,
};

use anyhow::{bail, Result};
use io::{PosixIo, SliceIo, Syscall, SyscallContext};
use risc0_circuit_rv32im::{
    layout::{OutBuffer, LAYOUT},
    REGISTER_GROUP_ACCUM, REGISTER_GROUP_CODE, REGISTER_GROUP_DATA,
};
use risc0_core::field::baby_bear::{BabyBear, BabyBearElem, BabyBearExtElem};
use risc0_zkp::{
    adapter::TapsProvider,
    core::hash::HashSuite,
    hal::{EvalCheck, Hal},
    layout::Buffer,
    prove::adapter::ProveAdapter,
};
use risc0_zkvm_platform::{
    fileno,
    memory::MEM_SIZE,
    syscall::{
        nr::{
            SYS_CYCLE_COUNT, SYS_GETENV, SYS_LOG, SYS_PANIC, SYS_READ, SYS_READ_AVAIL, SYS_WRITE,
        },
        reg_abi::{REG_A3, REG_A4},
        SyscallName,
    },
    WORD_SIZE,
};

use self::exec::{HostHandler, RV32Executor};
use crate::{
    binfmt::elf::Program,
    receipt::{insecure_skip_seal, Receipt},
    ControlId, MemoryImage, CIRCUIT, PAGE_SIZE,
};

const DEFAULT_SEGMENT_LIMIT_PO2: usize = 23; // 16M cycles

/// HAL creation functions for CUDA.
#[cfg(feature = "cuda")]
pub mod cuda {
    use std::rc::Rc;

    use risc0_circuit_rv32im::cuda::{CudaEvalCheckPoseidon, CudaEvalCheckSha256};
    use risc0_zkp::hal::cuda::{CudaHalPoseidon, CudaHalSha256};

    /// Returns the default SHA-256 HAL for the rv32im circuit.
    pub fn default_hal() -> (Rc<CudaHalSha256>, CudaEvalCheckSha256) {
        let hal = Rc::new(CudaHalSha256::new());
        let eval = CudaEvalCheckSha256::new(hal.clone());
        (hal, eval)
    }

    /// Returns the default Poseidon HAL for the rv32im circuit.
    pub fn default_poseidon_hal() -> (Rc<CudaHalPoseidon>, CudaEvalCheckPoseidon) {
        let hal = Rc::new(CudaHalPoseidon::new());
        let eval = CudaEvalCheckPoseidon::new(hal.clone());
        (hal, eval)
    }
}

/// HAL creation functions for Metal.
#[cfg(feature = "metal")]
pub mod metal {
    use std::rc::Rc;

    use risc0_circuit_rv32im::metal::{MetalEvalCheck, MetalEvalCheckSha256};
    use risc0_zkp::hal::metal::{MetalHalPoseidon, MetalHalSha256, MetalHashPoseidon};

    /// Returns the default SHA-256 HAL for the rv32im circuit.
    pub fn default_hal() -> (Rc<MetalHalSha256>, MetalEvalCheckSha256) {
        let hal = Rc::new(MetalHalSha256::new());
        let eval = MetalEvalCheckSha256::new(hal.clone());
        (hal, eval)
    }

    /// Returns the default Poseidon HAL for the rv32im circuit.
    pub fn default_poseidon_hal() -> (Rc<MetalHalPoseidon>, MetalEvalCheck<MetalHashPoseidon>) {
        let hal = Rc::new(MetalHalPoseidon::new());
        let eval = MetalEvalCheck::<MetalHashPoseidon>::new(hal.clone());
        (hal, eval)
    }
}

/// HAL creation functions for the CPU.
pub mod cpu {
    use std::rc::Rc;

    use risc0_circuit_rv32im::{cpu::CpuEvalCheck, CircuitImpl};
    use risc0_zkp::hal::cpu::{BabyBearPoseidonCpuHal, BabyBearSha256CpuHal};

    use crate::CIRCUIT;

    /// Returns the default SHA-256 HAL for the rv32im circuit.
    ///
    /// RISC Zero uses a
    /// [HAL](https://docs.rs/risc0-zkp/latest/risc0_zkp/hal/index.html)
    /// (Hardware Abstraction Layer) to interface with the zkVM circuit.
    /// This function returns the default HAL for the selected `risc0-zkvm`
    /// features. It also returns the associated
    /// [EvalCheck](https://docs.rs/risc0-zkp/latest/risc0_zkp/hal/trait.EvalCheck.html)
    /// used for computing the cryptographic check polynomial.
    ///
    /// Note that this function will return different types when
    /// `risc0-zkvm` is built with features that select different the target
    /// hardware. The version documented here is used when no special
    /// hardware features are selected.
    pub fn default_hal() -> (Rc<BabyBearSha256CpuHal>, CpuEvalCheck<'static, CircuitImpl>) {
        let hal = Rc::new(BabyBearSha256CpuHal::new());
        let eval = CpuEvalCheck::new(&CIRCUIT);
        (hal, eval)
    }

    /// Returns the default Poseidon HAL for the rv32im circuit.
    ///
    /// The same as [default_hal] except it gives the default HAL for
    /// securing the circuit using Poseidon (instead of SHA-256).
    pub fn default_poseidon_hal() -> (
        Rc<BabyBearPoseidonCpuHal>,
        CpuEvalCheck<'static, CircuitImpl>,
    ) {
        let hal = Rc::new(BabyBearPoseidonCpuHal::new());
        let eval = CpuEvalCheck::new(&CIRCUIT);
        (hal, eval)
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "cuda")] {
        pub use cuda::{default_hal, default_poseidon_hal};
    } else if #[cfg(feature = "metal")] {
        pub use metal::{default_hal, default_poseidon_hal};
    } else {
        pub use cpu::{default_hal, default_poseidon_hal};
    }
}

struct ReadToObj<'a, T: serde::de::DeserializeOwned> {
    obj: &'a mut T,
    buf: Vec<u8>,
}

impl<'a, T: serde::de::DeserializeOwned> Write for ReadToObj<'a, T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.flush()
    }
}

impl<'a, T: serde::de::DeserializeOwned> Drop for ReadToObj<'a, T> {
    fn drop(&mut self) {
        let aligned: Vec<u32> = self
            .buf
            .chunks(WORD_SIZE)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        *self.obj = crate::serde::from_slice(&aligned).unwrap();
    }
}

/// Options available to modify the prover's behavior.
pub struct ProverOpts<'a> {
    skip_seal: bool,

    skip_verify: bool,

    syscall_handlers: HashMap<String, Box<dyn Syscall + 'a>>,

    io: PosixIo<'a>,
    env_vars: HashMap<String, String>,
    trace_callback: Option<Box<dyn FnMut(TraceEvent) -> Result<()> + 'a>>,
    pub(crate) unknown_syscall_handler: Box<dyn Syscall + 'a>,

    preflight: bool,

    segment_limit_po2: usize,

    pub(crate) finalized: bool,
}

impl<'a> ProverOpts<'a> {
    /// Set the segment limit specified as a power of 2.
    ///
    /// When running a large program, this limit specifies the max cycles
    /// allowed for a particular segment.
    pub fn with_segment_limit_po2(self, segment_limit_po2: usize) -> Self {
        Self {
            segment_limit_po2,
            ..self
        }
    }

    /// If true, skip generating the seal in receipt. This should
    /// only be used for testing. In this case, performace will be
    /// much better but we will not be able to cryptographically
    /// verify the execution.
    pub fn with_skip_seal(self, skip_seal: bool) -> Self {
        Self { skip_seal, ..self }
    }

    /// If true, don't verify the seal after creating it. This
    /// is useful if you wish to use a non-standard verifier for
    /// example.
    pub fn with_skip_verify(self, skip_verify: bool) -> Self {
        Self {
            skip_verify,
            ..self
        }
    }

    /// EXPERIMENTAL: If this and skip_seal are both true, run using
    /// preflight instead of with the circuit. This feature is not
    /// yet complete. Alternatively, enable preflight by setting the
    /// `RISC0_EXPERIMENTAL_PREFLIGHT` environment variable.
    pub fn with_preflight(self, preflight: bool) -> Self {
        Self { preflight, ..self }
    }

    /// Add a handler for a syscall which inputs and outputs a slice
    /// of plain old data. The guest can call these by invoking
    /// `risc0_zkvm::guest::env::send_recv_slice`
    pub fn with_slice_io(self, syscall: SyscallName, handler: impl SliceIo + 'a) -> Self {
        self.with_syscall(syscall, handler.to_syscall())
    }

    /// Add a handler for a syscall which inputs and outputs a slice
    /// of plain old data. The guest can call these callbacks by
    /// invoking `risc0_zkvm::guest::env::send_recv_slice`.
    pub fn with_sendrecv_callback(
        self,
        syscall: SyscallName,
        f: impl Fn(&[u8]) -> Vec<u8> + 'a,
    ) -> Self {
        self.with_slice_io(syscall, io::slice_io_from_fn(f))
    }

    /// Add a handler for a raw syscall implementation. The guest can
    /// invoke these using the `risc0_zkvm_platform::syscall!` macro.
    pub fn with_syscall(mut self, syscall: SyscallName, handler: impl Syscall + 'a) -> Self {
        self.syscall_handlers
            .insert(syscall.as_str().to_string(), Box::new(handler));
        self
    }

    /// Provide a handler for when an unknown syscall is encountered.
    pub fn with_unknown_syscall_handler(mut self, handler: impl Syscall + 'a) -> Self {
        self.unknown_syscall_handler = Box::new(handler);
        self
    }

    /// Add a callback handler for raw trace messages.
    pub fn with_trace_callback(
        mut self,
        callback: impl FnMut(TraceEvent) -> Result<()> + 'a,
    ) -> Self {
        assert!(!self.trace_callback.is_some(), "Duplicate trace callback");
        self.trace_callback = Some(Box::new(callback));
        self
    }

    /// Add a posix-style standard input.
    pub fn with_stdin(self, reader: impl Read + 'a) -> Self {
        self.with_read_fd(fileno::STDIN, BufReader::new(reader))
    }

    /// Add a posix-style standard output.
    pub fn with_stdout(self, writer: impl Write + 'a) -> Self {
        self.with_write_fd(fileno::STDOUT, Box::new(writer))
    }

    /// Add a serialized object on standard input
    pub fn with_stdin_obj(self, obj: impl serde::Serialize) -> Self {
        let serialized = crate::serde::to_vec(&obj).unwrap();
        let bytes: Vec<u8> = bytemuck::cast_slice(&serialized).to_vec();
        self.with_stdin(Cursor::new(bytes))
    }

    /// Add an object to be deserialized from the guest's standadrd output
    pub fn with_stdout_obj(self, obj: &'a mut impl serde::de::DeserializeOwned) -> Self {
        self.with_write_fd(
            fileno::STDOUT,
            ReadToObj {
                obj,
                buf: Vec::new(),
            },
        )
    }

    /// Add a posix-style file descriptor for reading.
    pub fn with_read_fd(mut self, fd: u32, reader: impl BufRead + 'a) -> Self {
        self.io = self.io.with_read_fd(fd, Box::new(reader));
        self
    }

    /// Add a posix-style file descriptor for writing.
    pub fn with_write_fd(mut self, fd: u32, writer: impl Write + 'a) -> Self {
        self.io = self.io.with_write_fd(fd, Box::new(writer));
        self
    }

    /// Add an environment variable to the guest environment.
    pub fn with_env_var(mut self, name: &str, val: &str) -> Self {
        self.env_vars.insert(name.to_string(), val.to_string());
        self
    }

    /// Add late-binding handlers for constructed environment.
    fn finalize(mut self) -> Self {
        if self.finalized {
            self
        } else {
            self.finalized = true;
            let io = Rc::new(take(&mut self.io));
            let getenv = Getenv(take(&mut self.env_vars));
            self.with_syscall(SYS_READ, io.clone())
                .with_syscall(SYS_READ_AVAIL, io.clone())
                .with_syscall(SYS_WRITE, io)
                .with_syscall(SYS_GETENV, getenv)
        }
    }

    /// Returns an empty ProverOpts with none of the default system calls or
    /// file descriptors attached.
    pub fn without_defaults() -> Self {
        ProverOpts {
            io: PosixIo::new(),
            skip_seal: false,
            skip_verify: false,
            syscall_handlers: HashMap::new(),
            env_vars: HashMap::new(),
            trace_callback: None,
            preflight: false,
            unknown_syscall_handler: Box::new(UnknownSyscall),
            finalized: false,
            segment_limit_po2: DEFAULT_SEGMENT_LIMIT_PO2,
        }
    }
}

struct UnknownSyscall;

impl Syscall for UnknownSyscall {
    fn syscall(
        &self,
        syscall: &str,
        _ctx: &dyn SyscallContext,
        _to_guest: &mut [u32],
    ) -> Result<(u32, u32)> {
        panic!("Unknown syscall {syscall}")
    }
}

struct DefaultSyscall;

impl Syscall for DefaultSyscall {
    fn syscall(
        &self,
        syscall: &str,
        ctx: &dyn SyscallContext,
        _to_guest: &mut [u32],
    ) -> Result<(u32, u32)> {
        if syscall == SYS_PANIC.as_str() || syscall == SYS_LOG.as_str() {
            let buf_ptr = ctx.load_register(REG_A3);
            let buf_len = ctx.load_register(REG_A4);
            let from_guest = ctx.load_region(buf_ptr, buf_len);
            let msg = from_utf8(&from_guest)?;

            if syscall == SYS_PANIC.as_str() {
                bail!("Guest panicked: {msg}");
            } else if syscall == SYS_LOG.as_str() {
                println!("R0VM[{}] {}", ctx.get_cycle(), msg);
            } else {
                unreachable!()
            }
            Ok((0, 0))
        } else if syscall == SYS_CYCLE_COUNT.as_str() {
            Ok((ctx.get_cycle() as u32, 0))
        } else {
            bail!("Unknown syscall: {syscall}")
        }
    }
}

struct Getenv(HashMap<String, String>);

impl Syscall for Getenv {
    fn syscall(
        &self,
        _syscall: &str,
        ctx: &dyn SyscallContext,
        to_guest: &mut [u32],
    ) -> Result<(u32, u32)> {
        let buf_ptr = ctx.load_register(REG_A3);
        let buf_len = ctx.load_register(REG_A4);
        let from_guest = ctx.load_region(buf_ptr, buf_len);
        let msg = from_utf8(&from_guest)?;

        match self.0.get(msg) {
            None => Ok((u32::MAX, 0)),
            Some(val) => {
                let nbytes = min(to_guest.len() * WORD_SIZE, val.as_bytes().len());
                let to_guest_u8s: &mut [u8] = bytemuck::cast_slice_mut(to_guest);
                to_guest_u8s[0..nbytes].clone_from_slice(&val.as_bytes()[0..nbytes]);
                Ok((val.as_bytes().len() as u32, 0))
            }
        }
    }
}

impl<'a> Default for ProverOpts<'a> {
    fn default() -> ProverOpts<'a> {
        Self::without_defaults()
            .with_preflight(std::env::var("RISC0_EXPERIMENTAL_PREFLIGHT").is_ok())
            .with_read_fd(fileno::STDIN, BufReader::new(stdin()))
            .with_write_fd(fileno::STDOUT, stdout())
            .with_write_fd(fileno::STDERR, stderr())
            .with_syscall(SYS_PANIC, DefaultSyscall)
            .with_syscall(SYS_LOG, DefaultSyscall)
            .with_syscall(SYS_CYCLE_COUNT, DefaultSyscall)
    }
}

/// Manages communication with and execution of a zkVM [Program]
///
/// # Usage
/// A [Prover] is constructed from the ELF code and an Image ID generated from
/// the guest code to be proven (see
/// [risc0_build](https://docs.rs/risc0-build/latest/risc0_build/) for more
/// information about how these are generated). Use [Prover::new] if you want
/// the default [ProverOpts], or [Prover::new_with_opts] to use custom options.
/// ```ignore
/// // In real code, the ELF & Image ID would be generated by risc0 build scripts from guest code
/// use methods::{EXAMPLE_ELF, EXAMPLE_ID};
/// use risc0_zkvm::Prover;
///
/// let mut prover = Prover::new(&EXAMPLE_ELF, EXAMPLE_ID)?;
/// ```
/// Provers should essentially always be mutable so that their [Prover::run]
/// method may be called.
///
/// Input data can be passed to the Prover with [Prover::add_input_u32_slice]
/// (or [Prover::add_input_u8_slice]). After all inputs have been added, call
/// [Prover::run] to execute the guest code and produce a [Receipt] proving
/// execution.
/// ```ignore
/// prover.add_input_u32_slice(&risc0_zkvm::serde::to_vec(&input)?);
/// let receipt = prover.run()?;
/// ```
/// After running the prover, publicly proven results can be accessed from the
/// [Receipt].
/// ```ignore
/// let receipt = prover.run()?;
/// let proven_result: ResultType = risc0_zkvm::serde::from_slice(&receipt.journal)?;
/// ```
pub struct Prover<'a> {
    inner: ProverImpl<'a>,
    image: Rc<RefCell<MemoryImage>>,
    pc: u32,
    preflight_segments: Option<Box<dyn Iterator<Item = Result<preflight::Segment>> + 'a>>,

    /// How many cycles executing the guest took.
    ///
    /// Initialized to 0 by [Prover::new], then computed when [Prover::run] is
    /// called. Note that this is privately shared with the host; it is not
    /// present in the [Receipt].
    pub cycles: usize,

    /// The exit code reported by the latest segment execution. The possible
    /// values are:
    /// - 0: Halted normally
    /// - 1: User-initiated pause
    /// - 2: System-initiated split
    pub exit_code: u32,
}

impl<'a> Prover<'a> {
    /// Construct a new prover using the default options.
    ///
    /// This will return an `Err` if `elf` is not a valid ELF file.
    pub fn new(elf: &[u8]) -> Result<Self> {
        Self::new_with_opts(elf, ProverOpts::default())
    }

    /// Construct a new prover using custom [ProverOpts].
    ///
    /// This will return an `Err` if `elf` is not a valid ELF file.
    pub fn new_with_opts(elf: &[u8], opts: ProverOpts<'a>) -> Result<Self> {
        let program = Program::load_elf(&elf, MEM_SIZE as u32)?;
        Ok(Prover {
            inner: ProverImpl::new(opts),
            image: Rc::new(RefCell::new(MemoryImage::new(&program, PAGE_SIZE as u32))),
            pc: program.entry,
            cycles: 0,
            preflight_segments: None,
            exit_code: 0,
        })
    }

    /// Construct a prover from a memory image.
    pub fn from_image(image: Rc<RefCell<MemoryImage>>, pc: u32, opts: ProverOpts<'a>) -> Self {
        Prover {
            inner: ProverImpl::new(opts),
            image,
            pc,
            cycles: 0,
            preflight_segments: None,
            exit_code: 0,
        }
    }

    /// Provide input data to the guest. This data can be read by the guest
    /// via [crate::guest::env::read].
    ///
    /// It is possible to provide multiple inputs to the guest so long as the
    /// guest reads them in the same order they are added by the [Prover].
    /// However, to reduce maintenance burden and the chance of mistakes, we
    /// recommend instead using a single `struct` to hold all the inputs and
    /// calling [Prover::add_input_u8_slice] just once (on the serialized
    /// representation of that input).
    pub fn add_input_u8_slice(&mut self, slice: &[u8]) {
        self.inner.input.extend_from_slice(slice);
    }

    /// Provide input data to the guest. This data can be read by the guest
    /// via [crate::guest::env::read].
    ///
    /// It is possible to provide multiple inputs to the guest so long as the
    /// guest reads them in the same order they are added by the [Prover].
    /// However, to reduce maintenance burden and the chance of mistakes, we
    /// recommend instead using a single `struct` to hold all the inputs and
    /// calling [Prover::add_input_u32_slice] just once (on the serialized
    /// representation of that input).
    pub fn add_input_u32_slice(&mut self, slice: &[u32]) {
        self.inner
            .input
            .extend_from_slice(bytemuck::cast_slice(slice));
    }

    /// Run the guest code. If the guest exits successfully, this returns a
    /// [Receipt] that proves execution. If the execution of the guest fails for
    /// any reason, this instead returns an `Err`.
    ///
    /// This function uses the default HAL (Hardware Abstraction Layer) to
    /// run the guest. If you want to use a different HAL, you can do so either
    /// by changing the default using risc0_zkvm feature flags, or by using
    /// [Prover::run_with_hal].
    #[tracing::instrument(skip_all)]
    pub fn run(&mut self) -> Result<Receipt> {
        let (hal, eval) = default_hal();
        cfg_if::cfg_if! {
            if #[cfg(feature = "dual")] {
                let cpu_hal = risc0_zkp::hal::cpu::BabyBearSha256CpuHal::new();
                let cpu_eval = risc0_circuit_rv32im::cpu::CpuEvalCheck::new(&CIRCUIT);
                let hal = risc0_zkp::hal::dual::DualHal::new(hal.as_ref(), &cpu_hal);
                let eval = risc0_zkp::hal::dual::DualEvalCheck::new(eval, &cpu_eval);
                self.run_with_hal(&hal, &eval)
            } else {
                self.run_with_hal(hal.as_ref(), &eval)
            }
        }
    }

    /// Run the guest code. Like [Prover::run], but with parameters for
    /// selecting a HAL, allowing the use of HALs other than [default_hal].
    /// People creating or using a third-party HAL can use this function to run
    /// the Prover with that HAL.
    #[tracing::instrument(skip_all)]
    pub fn run_with_hal<H, E>(&mut self, hal: &H, eval: &E) -> Result<Receipt>
    where
        H: Hal<Field = BabyBear, Elem = BabyBearElem, ExtElem = BabyBearExtElem>,
        <<H as Hal>::HashSuite as HashSuite<BabyBear>>::HashFn: ControlId,
        E: EvalCheck<H>,
    {
        if !self.inner.opts_finalized {
            let mut opts = take(&mut self.inner.opts);
            if !self.inner.input.is_empty() {
                // TODO: Remove add_input_*_slice in favor of with_stdin,
                // and eliminate this "input" field.
                opts = opts.with_stdin(Cursor::new(take(&mut self.inner.input)))
            }
            self.inner.opts = opts.finalize();
            self.inner.opts_finalized = true;
        } else {
            // Continuation; opts was finalized the previous run.  Make sure we didn't get
            // any more input.
            assert!(self.inner.input.is_empty(), "Input may not be added after the prover starts proving using the add_input_* calls");
        }
        let skip_seal = self.inner.opts.skip_seal || insecure_skip_seal();
        let segment_limit_po2 = self.inner.opts.segment_limit_po2;

        if self.inner.opts.preflight {
            let segments = self.preflight_segments.get_or_insert_with(|| {
                let opts = take(&mut self.inner.opts);
                // TODO: avoid image clone
                let exec =
                    preflight::exec::ExecState::new(self.pc, self.image.borrow().clone(), opts);

                Box::new(exec.segmentize())
            });

            let segment = segments
                .next()
                .expect("Ran out of segments but user still wants more!")?;
            let journal = self.inner.journal.buf.take();
            let seal = if skip_seal {
                Vec::new()
            } else {
                segment.prove_with_hal(hal, eval)?.seal
            };
            return Ok(Receipt { journal, seal });
        }

        let image_id = self.image.borrow().get_root();
        let mut executor = RV32Executor::new(
            &CIRCUIT,
            Rc::clone(&self.image),
            self.pc,
            &mut self.inner,
            segment_limit_po2,
        );
        let (cycles, exit_code, pc) = executor.run()?;
        self.cycles = cycles;
        self.exit_code = exit_code;
        self.pc = pc;

        let mut adapter = ProveAdapter::new(&mut executor.executor);
        let mut prover = risc0_zkp::prove::Prover::new(hal, CIRCUIT.get_taps());

        adapter.execute(prover.iop());

        let seal = if skip_seal {
            Vec::new()
        } else {
            prover.set_po2(adapter.po2() as usize);

            prover.commit_group(
                REGISTER_GROUP_CODE,
                hal.copy_from_elem("code", &adapter.get_code().as_slice()),
            );
            prover.commit_group(
                REGISTER_GROUP_DATA,
                hal.copy_from_elem("data", &adapter.get_data().as_slice()),
            );
            adapter.accumulate(prover.iop());
            prover.commit_group(
                REGISTER_GROUP_ACCUM,
                hal.copy_from_elem("accum", &adapter.get_accum().as_slice()),
            );

            let mix = hal.copy_from_elem("mix", &adapter.get_mix().as_slice());
            let out_slice = &adapter.get_io().as_slice();

            log::debug!("Globals: {:?}", OutBuffer(out_slice).tree(&LAYOUT));

            let out = hal.copy_from_elem("out", &adapter.get_io().as_slice());

            prover.finalize(&[&mix, &out], eval)
        };

        // Attach the full version of the output journal & construct receipt object
        let journal = self.inner.journal.buf.take();
        let receipt = Receipt { journal, seal };

        if !skip_seal && !self.inner.opts.skip_verify {
            // Verify receipt to make sure it works
            receipt.verify_with_hash::<H::HashSuite, _>(&image_id)?;
        }

        Ok(receipt)
    }
}

// Capture the journal output in a buffer that we can access afterwards.
#[derive(Clone, Default)]
pub(crate) struct Journal {
    buf: Rc<RefCell<Vec<u8>>>,
}

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.borrow_mut().flush()
    }
}

struct ProverImpl<'a> {
    pub input: Vec<u8>,
    pub journal: Journal,
    pub opts: ProverOpts<'a>,

    // True if we've already called finalize() on the ProverOpts.
    // TODO: If we get rid of the add_input* calls, this should be unnecessary.
    opts_finalized: bool,
}

impl<'a> ProverImpl<'a> {
    fn new(opts: ProverOpts<'a>) -> Self {
        let journal = Journal::default();
        let opts = opts.with_write_fd(fileno::JOURNAL, journal.clone());
        Self {
            input: Vec::new(),
            journal,
            opts,
            opts_finalized: false,
        }
    }
}

impl<'a> HostHandler for ProverImpl<'a> {
    fn on_txrx(
        &mut self,
        ctx: &dyn SyscallContext,
        syscall: &str,
        to_guest: &mut [u32],
    ) -> Result<(u32, u32)> {
        log::debug!("syscall {syscall}, {} words to guest", to_guest.len());
        if let Some(cb) = self.opts.syscall_handlers.get(syscall) {
            return cb.syscall(syscall, ctx, to_guest);
        }
        // TODO: Use the standard syscall handler framework for this instead of matching
        // on name.
        match syscall
            .strip_prefix("risc0_zkvm_platform::syscall::nr::")
            .unwrap_or(syscall)
        {
            "SYS_RANDOM" => {
                log::debug!("SYS_RANDOM: {}", to_guest.len());
                let mut rand_buf = vec![0u8; to_guest.len() * WORD_SIZE];
                getrandom::getrandom(rand_buf.as_mut_slice())?;
                bytemuck::cast_slice_mut(to_guest).clone_from_slice(rand_buf.as_slice());
                Ok((0, 0))
            }
            _ => self
                .opts
                .unknown_syscall_handler
                .syscall(syscall, ctx, to_guest),
        }
    }

    fn is_trace_enabled(&self) -> bool {
        self.opts.trace_callback.is_some()
    }

    fn on_trace(&mut self, event: TraceEvent) -> Result<()> {
        if let Some(ref mut cb) = self.opts.trace_callback {
            cb(event)
        } else {
            Ok(())
        }
    }
}

/// An event traced from the running VM.
#[non_exhaustive]
#[derive(PartialEq)]
pub enum TraceEvent {
    /// An instruction has started at the given program counter
    InstructionStart {
        /// Cycle number since startup
        cycle: u32,
        /// Program counter of the instruction being executed
        pc: u32,
    },

    /// A register has been set
    RegisterSet {
        /// Register ID (0-16)
        reg: usize,
        /// New value in the register
        value: u32,
    },

    /// A memory location has been written
    MemorySet {
        /// Address of word that's been written
        addr: u32,
        /// Value of word that's been written
        value: u32,
    },
}

impl Debug for TraceEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InstructionStart { cycle, pc } => {
                write!(f, "InstructionStart({cycle}, 0x{pc:08X})")
            }
            Self::RegisterSet { reg, value } => write!(f, "RegisterSet({reg}, 0x{value:08X})"),
            Self::MemorySet { addr, value } => write!(f, "MemorySet(0x{addr:08X}, 0x{value:08X})"),
        }
    }
}
