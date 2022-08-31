use crate::engine::system::SyscallId;
use byteorder::{ByteOrder, LittleEndian};
use log::{debug, info, trace};
use riscu::{types::*, DecodedProgram, Instruction, Register};
use std::io::{self, Write};
use std::mem::size_of;

//
// Public Interface
//

pub type EmulatorValue = u64;

#[derive(Debug)]
pub struct EmulatorState {
    registers: Vec<EmulatorValue>,
    memory: Vec<EmulatorValue>,
    program_counter: EmulatorValue,
    program_break: EmulatorValue,
    running: bool,
}

impl EmulatorState {
    pub fn new(memory_size: usize) -> Self {
        Self {
            registers: vec![0; NUMBER_OF_REGISTERS],
            memory: vec![0; memory_size / riscu::WORD_SIZE],
            program_counter: 0,
            program_break: 0,
            running: false,
        }
    }

    pub fn run(&mut self, program: &DecodedProgram, argv: &[String]) {
        let sp_value = self.memory.len() * riscu::WORD_SIZE;
        self.set_reg(Register::Sp, sp_value as u64);
        self.program_counter = program.code.address;
        self.program_break = initial_program_break(program);
        self.load_data_segment(program);
        self.load_stack_segment(argv);
        self.running = true;
        while self.running {
            let instr = fetch_and_decode(self, program);
            execute(self, instr);
        }
    }
}

//
// Private Implementation
//

const PAGE_SIZE: u64 = 4 * 1024;
const NUMBER_OF_REGISTERS: usize = 32;
const INSTRUCTION_SIZE_MASK: u64 = riscu::INSTRUCTION_SIZE as u64 - 1;
const WORD_SIZE_MASK: u64 = riscu::WORD_SIZE as u64 - 1;

fn next_multiple_of(value: u64, align: u64) -> u64 {
    ((value + (align - 1)) / align) * align
}

fn initial_program_break(program: &DecodedProgram) -> EmulatorValue {
    let data_size = program.data.content.len() * riscu::WORD_SIZE;
    let data_end = program.data.address + data_size as u64;
    next_multiple_of(data_end, PAGE_SIZE)
}

impl EmulatorState {
    fn pc_add(&mut self, imm: u64) {
        self.program_counter = self.program_counter.wrapping_add(imm);
    }

    fn pc_next(&mut self) {
        self.pc_add(riscu::INSTRUCTION_SIZE as u64);
    }

    fn pc_set(&mut self, val: EmulatorValue) {
        assert!(val & INSTRUCTION_SIZE_MASK == 0, "program counter aligned");
        self.program_counter = val;
    }

    fn get_reg(&self, reg: Register) -> EmulatorValue {
        self.registers[reg as usize]
    }

    fn set_reg(&mut self, reg: Register, val: EmulatorValue) {
        assert!(reg != Register::Zero, "cannot set `zero` register");
        self.registers[reg as usize] = val;
    }

    fn set_reg_maybe(&mut self, reg: Register, val: EmulatorValue) {
        if reg == Register::Zero {
            return;
        };
        self.set_reg(reg, val);
    }

    fn get_mem(&self, adr: EmulatorValue) -> EmulatorValue {
        self.memory[adr as usize / riscu::WORD_SIZE]
    }

    fn get_mem_maybe(&self, adr: EmulatorValue) -> Option<EmulatorValue> {
        self.memory.get(adr as usize / riscu::WORD_SIZE).cloned()
    }

    fn set_mem(&mut self, adr: EmulatorValue, val: EmulatorValue) {
        self.memory[adr as usize / riscu::WORD_SIZE] = val;
    }

    fn push_stack(&mut self, val: EmulatorValue) {
        let sp = self.get_reg(Register::Sp) - riscu::WORD_SIZE as u64;
        self.set_reg(Register::Sp, sp);
        self.set_mem(sp, val);
    }

    fn load_data_segment(&mut self, program: &DecodedProgram) {
        for (i, val) in program.data.content.iter().enumerate() {
            let adr = program.data.address as usize + i * riscu::WORD_SIZE;
            self.set_mem(adr as u64, *val);
        }
    }

    // Prepares arguments on the stack like a UNIX system. Note that we
    // pass an empty environment and that all strings will be properly
    // zero-terminated and word-aligned:
    //
    // | argc | argv[0] | ... | argv[n] | 0 | env[0] | ... | env[m] | 0 |
    //
    fn load_stack_segment(&mut self, argv: &[String]) {
        let argc = argv.len() as EmulatorValue;
        debug!("argc: {}, argv: {:?}", argc, argv);
        let argv_ptrs: Vec<EmulatorValue> = argv
            .iter()
            .rev()
            .map(|arg| {
                let c_string = arg.to_owned() + "\0\0\0\0\0\0\0\0";
                for chunk in c_string.as_bytes().chunks_exact(size_of::<u64>()).rev() {
                    self.push_stack(LittleEndian::read_u64(chunk));
                }
                self.get_reg(Register::Sp)
            })
            .collect();
        self.push_stack(0); // terminate env table
        self.push_stack(0); // terminate argv table
        for argv_ptr in argv_ptrs {
            self.push_stack(argv_ptr);
        }
        self.push_stack(argc);
    }
}

fn fetch_and_decode(state: &mut EmulatorState, program: &DecodedProgram) -> Instruction {
    assert!(state.program_counter & INSTRUCTION_SIZE_MASK == 0);
    let offset = state.program_counter - program.code.address;
    program.code.content[offset as usize / riscu::INSTRUCTION_SIZE]
}

fn execute(state: &mut EmulatorState, instr: Instruction) {
    match instr {
        Instruction::Lui(utype) => exec_lui(state, utype),
        Instruction::Jal(jtype) => exec_jal(state, jtype),
        Instruction::Jalr(itype) => exec_jalr(state, itype),
        Instruction::Beq(btype) => exec_beq(state, btype),
        Instruction::Ld(itype) => exec_ld(state, itype),
        Instruction::Sd(stype) => exec_sd(state, stype),
        Instruction::Addi(itype) => exec_addi(state, itype),
        Instruction::Add(rtype) => exec_add(state, rtype),
        Instruction::Sub(rtype) => exec_sub(state, rtype),
        Instruction::Sltu(rtype) => exec_sltu(state, rtype),
        Instruction::Mul(rtype) => exec_mul(state, rtype),
        Instruction::Divu(rtype) => exec_divu(state, rtype),
        Instruction::Remu(rtype) => exec_remu(state, rtype),
        Instruction::Ecall(_itype) => exec_ecall(state),
    }
}

fn exec_lui(state: &mut EmulatorState, utype: UType) {
    let rd_value = ((utype.imm() as i32) << 12) as u64;
    trace_utype(state, "lui", utype, rd_value);
    state.set_reg(utype.rd(), rd_value);
    state.pc_next();
}

fn exec_jal(state: &mut EmulatorState, jtype: JType) {
    let rd_value = state.program_counter + riscu::INSTRUCTION_SIZE as u64;
    trace_jtype(state, "jal", jtype, rd_value);
    state.set_reg_maybe(jtype.rd(), rd_value);
    state.pc_add(jtype.imm() as u64);
}

fn exec_jalr(state: &mut EmulatorState, itype: IType) {
    let rs1_value = state.get_reg(itype.rs1());
    let rd_value = state.program_counter + riscu::INSTRUCTION_SIZE as u64;
    let pc_value = rs1_value.wrapping_add(itype.imm() as u64);
    trace_itype(state, "jalr", itype, rd_value);
    state.set_reg_maybe(itype.rd(), rd_value);
    state.pc_set(pc_value);
}

fn exec_beq(state: &mut EmulatorState, btype: BType) {
    let rs1_value = state.get_reg(btype.rs1());
    let rs2_value = state.get_reg(btype.rs2());
    trace_btype(state, "beq", btype);
    if rs1_value == rs2_value {
        state.pc_add(btype.imm() as u64);
    } else {
        state.pc_next();
    }
}

fn exec_ld(state: &mut EmulatorState, itype: IType) {
    let rs1_value = state.get_reg(itype.rs1());
    let address = rs1_value.wrapping_add(itype.imm() as u64);
    let rd_value = state.get_mem(address);
    trace_itype(state, "ld", itype, rd_value);
    state.set_reg(itype.rd(), rd_value);
    state.pc_next();
}

fn exec_sd(state: &mut EmulatorState, stype: SType) {
    let rs1_value = state.get_reg(stype.rs1());
    let rs2_value = state.get_reg(stype.rs2());
    let address = rs1_value.wrapping_add(stype.imm() as u64);
    trace_stype(state, "sd", stype, address);
    state.set_mem(address, rs2_value);
    state.pc_next();
}

fn exec_addi(state: &mut EmulatorState, itype: IType) {
    let rs1_value = state.get_reg(itype.rs1());
    let rd_value = rs1_value.wrapping_add(itype.imm() as u64);
    trace_itype(state, "addi", itype, rd_value);
    state.set_reg(itype.rd(), rd_value);
    state.pc_next();
}

fn exec_add(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    let rd_value = rs1_value.wrapping_add(rs2_value);
    trace_rtype(state, "add", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_sub(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    let rd_value = rs1_value.wrapping_sub(rs2_value);
    trace_rtype(state, "sub", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_sltu(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    let rd_value = if rs1_value < rs2_value { 1 } else { 0 };
    trace_rtype(state, "sltu", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_mul(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    let rd_value = rs1_value.wrapping_mul(rs2_value);
    trace_rtype(state, "mul", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_divu(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    assert!(rs2_value != 0, "check for non-zero divisor");
    let rd_value = rs1_value.wrapping_div(rs2_value);
    trace_rtype(state, "divu", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_remu(state: &mut EmulatorState, rtype: RType) {
    let rs1_value = state.get_reg(rtype.rs1());
    let rs2_value = state.get_reg(rtype.rs2());
    assert!(rs2_value != 0, "check for non-zero divisor");
    let rd_value = rs1_value.wrapping_rem(rs2_value);
    trace_rtype(state, "remu", rtype, rd_value);
    state.set_reg(rtype.rd(), rd_value);
    state.pc_next();
}

fn exec_ecall(state: &mut EmulatorState) {
    let a7_value = state.get_reg(Register::A7);
    if a7_value == SyscallId::Exit as u64 {
        let exit_code = state.get_reg(Register::A0);
        info!("program exiting with exit code {}", exit_code);
        state.running = false;
    } else if a7_value == SyscallId::Read as u64 {
        syscall_read(state);
    } else if a7_value == SyscallId::Write as u64 {
        syscall_write(state);
    } else if a7_value == SyscallId::Openat as u64 {
        syscall_openat(state);
    } else if a7_value == SyscallId::Brk as u64 {
        syscall_brk(state);
    } else {
        unimplemented!("unknown system call: {}", a7_value);
    }
    state.pc_next();
}

fn syscall_read(_state: &mut EmulatorState) {
    // TODO: Implement `read` system call.
    unimplemented!("missing `read` system call");
}

fn syscall_write(state: &mut EmulatorState) {
    let fd = state.get_reg(Register::A0);
    let buffer = state.get_reg(Register::A1);
    let size = state.get_reg(Register::A2);

    let result = 1;
    let data_start = buffer;
    let data_end = buffer + size;
    assert!(fd == 1, "only STDOUT file descriptor supported");
    (data_start..data_end)
        .step_by(size_of::<u64>())
        .for_each(|adr| {
            io::stdout()
                .write_all(&state.get_mem(adr).to_le_bytes())
                .unwrap();
        });
    io::stdout().flush().unwrap();

    state.set_reg(Register::A0, result);
    debug!("write({}, {:#x}, {}) -> {}", fd, buffer, size, result);
}

fn syscall_openat(_state: &mut EmulatorState) {
    // TODO: Implement `openat` system call.
    unimplemented!("missing `openat` system call");
}

fn syscall_brk(state: &mut EmulatorState) {
    let address = state.get_reg(Register::A0);

    // Check provided address is valid and falls between the current
    // program break (highest heap) and `sp` register (lowest stack).
    assert!(address & WORD_SIZE_MASK == 0, "program break aligned");
    if (address >= state.program_break) && (address < state.get_reg(Register::Sp)) {
        state.program_break = address;
    }
    let result = state.program_break;

    state.set_reg(Register::A0, result);
    debug!("brk({:#x}) -> {:#x}", address, result);
}

fn trace_btype(state: &EmulatorState, mne: &str, btype: BType) {
    trace!(
        "pc={:#x}: {} {:?},{:?},{}: {:?}={:#x}, {:?}={:#x} |- {}",
        state.program_counter,
        mne,
        btype.rs1(),
        btype.rs2(),
        btype.imm(),
        btype.rs1(),
        state.get_reg(btype.rs1()),
        btype.rs2(),
        state.get_reg(btype.rs2()),
        state.get_reg(btype.rs1()) == state.get_reg(btype.rs2())
    );
}

fn trace_itype(state: &EmulatorState, mne: &str, itype: IType, rd_value: EmulatorValue) {
    trace!(
        "pc={:#x}: {} {:?},{:?},{}: {:?}={:#x} |- {:?}={:#x} -> {:?}={:#x}",
        state.program_counter,
        mne,
        itype.rd(),
        itype.rs1(),
        itype.imm(),
        itype.rs1(),
        state.get_reg(itype.rs1()),
        itype.rd(),
        state.get_reg(itype.rd()),
        itype.rd(),
        rd_value
    );
}

fn trace_jtype(state: &EmulatorState, mne: &str, jtype: JType, rd_value: EmulatorValue) {
    trace!(
        "pc={:#x}: {} {:?},{}: |- {:?}={:#x} -> {:?}={:#x}",
        state.program_counter,
        mne,
        jtype.rd(),
        jtype.imm(),
        jtype.rd(),
        state.get_reg(jtype.rd()),
        jtype.rd(),
        rd_value
    );
}

fn trace_rtype(state: &EmulatorState, mne: &str, rtype: RType, rd_value: EmulatorValue) {
    trace!(
        "pc={:#x}: {} {:?},{:?},{:?}: {:?}={:#x}, {:?}={:#x} |- {:?}={:#x} -> {:?}={:#x}",
        state.program_counter,
        mne,
        rtype.rd(),
        rtype.rs1(),
        rtype.rs2(),
        rtype.rs1(),
        state.get_reg(rtype.rs1()),
        rtype.rs2(),
        state.get_reg(rtype.rs2()),
        rtype.rd(),
        state.get_reg(rtype.rd()),
        rtype.rd(),
        rd_value
    );
}

fn trace_stype(state: &EmulatorState, mne: &str, stype: SType, address: EmulatorValue) {
    trace!(
        "pc={:#x}: {} {:?},{}({:?}): {:?}={:#x}, {:?}={:#x} |- mem[{:#x}]={:#x} -> mem[{:#x}]={:#x}",
        state.program_counter,
        mne,
        stype.rs2(),
        stype.imm(),
        stype.rs1(),
        stype.rs1(),
        state.get_reg(stype.rs1()),
        stype.rs2(),
        state.get_reg(stype.rs2()),
        address,
        state.get_mem_maybe(address).unwrap_or(0),
        address,
        state.get_reg(stype.rs2())
    );
}

fn trace_utype(state: &EmulatorState, mne: &str, utype: UType, rd_value: EmulatorValue) {
    trace!(
        "pc={:#x}: {} {:?},{:#x}: |- {:?}={:#x} -> {:?}={:#x}",
        state.program_counter,
        mne,
        utype.rd(),
        utype.imm(),
        utype.rd(),
        state.get_reg(utype.rd()),
        utype.rd(),
        rd_value
    );
}