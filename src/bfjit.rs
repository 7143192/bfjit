use crate::bfir::{self, BfIR};
use crate::error::{Result, RuntimeError, VMError};

use std::io::{Read, Write};
use std::path::Path;
use std::ptr;

use BfIR::*;

use dynasm::dynasm;
use dynasmrt::{DynasmApi, DynasmLabelApi};

const MEMORY_SIZE: usize = 4 * 1024 * 1024;

pub struct BFVM<'io> { // 'io为生命周期声明
    code: dynasmrt::ExecutableBuffer,
    start: dynasmrt::AssemblyOffset,
    memory: Box<[u8]>, // 智能指针，指向heap上的u8数组
    input: Box<dyn Read + 'io>, //智能指针，指向的是Read方法
    output: Box<dyn Write + 'io>,
}

#[inline(always)]
fn vm_error(re: RuntimeError) -> *mut VMError {
    let e = Box::new(VMError::from(re));
    Box::into_raw(e)
}

impl<'io> BFVM<'io> {
    // 用输入流、输出流、源文件路径初始化虚拟机
    unsafe extern "sysv64" fn getbyte(this: *mut Self, ptr: *mut u8)-> *mut VMError {
        let mut buf = [0_u8];
        let this = &mut *this;
        match this.input.read(&mut buf) {
            Ok(0) => {},
            Ok(1) => *ptr = buf[1],
            Err(e) => return vm_error(RuntimeError::IO(e)),
            _ => unreachable!(),
        }
        ptr::null_mut()
    }

    unsafe extern "sysv64" fn putbyte(this: *mut Self, ptr: *const u8) -> *mut VMError {
        let buf = std::slice::from_ref(&*ptr);
        let this = &mut *this;
        match this.output.write_all(buf) {
            Ok(()) => ptr::null_mut(),
            Err(e) => vm_error(RuntimeError::IO(e)),
        }
    }

    unsafe extern "sysv64" fn overflow_error() -> *mut VMError {
        vm_error(RuntimeError::PointerOverflow)
    }

    // this:         rdi r12
    // memory_start: rsi r13
    // memory_end:   rdx r14
    // ptr:          rcx r15
    fn compile(code: &[BfIR]) -> Result<(dynasmrt::ExecutableBuffer, dynasmrt::AssemblyOffset)> {
        let mut ops = dynasmrt::x64::Assembler::new()?; // 初始化compiler
        let start = ops.offset();
        let mut loops = vec![];
        dynasm!(ops
            ; push rax
            ; mov r12, rdi   // save this
            ; mov r13, rsi   // save memory_start
            ; mov r14, rdx   // save memory_end
            ; mov rcx, rsi   // ptr = memory_start
        );
        for &ir in code {
            match ir {
                AddPtr(x) => dynasm!(ops
                    ; add rcx, x as i32     // ptr += x
                    ; jc  ->overflow        // jmp if overflow
                    ; cmp rcx, r14          // ptr - memory_end
                    ; jnb ->overflow        // jmp if ptr >= memory_end
                ),
                SubPtr(x) => dynasm!(ops
                    ; sub rcx, x as i32     // ptr -= x
                    ; jc  ->overflow        // jmp if overflow
                    ; cmp rcx, r13          // ptr - memory_start
                    ; jb  ->overflow        // jmp if ptr < memory_start
                ),
                AddVal(x) => dynasm!(ops
                    ; add BYTE [rcx], x as i8    // *ptr += x
                ),
                SubVal(x) => dynasm!(ops
                    ; sub BYTE [rcx], x as i8    // *ptr -= x
                ),
                GetByte => dynasm!(ops
                    ; mov  r15, rcx         // save ptr
                    ; mov  rdi, r12
                    ; mov  rsi, rcx         // arg0: this, arg1: ptr
                    ; mov  rax, QWORD BFVM::getbyte as _
                    ; call rax              // getbyte(this, ptr)
                    ; test rax, rax
                    ; jnz  ->io_error       // jmp if rax != 0
                    ; mov  rcx, r15         // recover ptr
                ),
                PutByte => dynasm!(ops
                    ; mov  r15, rcx         // save ptr
                    ; mov  rdi, r12
                    ; mov  rsi, rcx         // arg0: this, arg1: ptr
                    ; mov  rax, QWORD BFVM::putbyte as _
                    ; call rax              // putbyte(this, ptr)
                    ; test rax, rax
                    ; jnz  ->io_error       // jmp if rax != 0
                    ; mov  rcx, r15         // recover ptr
                ),
                Jz => {
                    let left = ops.new_dynamic_label();
                    let right = ops.new_dynamic_label();
                    loops.push((left, right));

                    dynasm!(ops
                        ; cmp BYTE [rcx], 0
                        ; jz => right       // jmp if *ptr == 0
                        ; => left
                    )
                }
                Jnz => {
                    let (left, right) = loops.pop().unwrap();
                    dynasm!(ops
                        ; cmp BYTE [rcx], 0
                        ; jnz => left       // jmp if *ptr != 0
                        ; => right
                    )
                } 
            }
        }
        dynasm!(ops
            ; xor rax, rax
            ; jmp >exit
            ; -> overflow:
            ; mov rax, QWORD BFVM::overflow_error as _
            ; call rax
            ; jmp >exit
            ; -> io_error:
            ; exit:
            ; pop rdx
            ; ret
        );
        let code = ops.finalize().unwrap();
        Ok((code, start))
    }

    pub fn new(
        file_path: &Path,
        input: Box<dyn Read + 'io>,
        output: Box<dyn Write + 'io>,
        optimize: bool,
    ) -> Result<Self> {
        let src = std::fs::read_to_string(&file_path)?;
        let mut ir = bfir::compile(&src)?;
        drop(src); //删除string,这里希望再出作用域之前提前删除
        let (code, start) = Self::compile(&ir)?;
        drop(ir);
        let memory = vec![0; MEMORY_SIZE].into_boxed_slice();
        Ok(Self {
            code,
            start,
            memory,
            input,
            output,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        type RawFn = unsafe extern "sysv64" fn(
            this: *mut BFVM<'_>,
            memory_start: *mut u8,
            memory_end: *const u8,
        ) -> *mut VMError;

        let raw_fn: RawFn = unsafe { std::mem::transmute(self.code.ptr(self.start)) };

        let this: *mut Self = self;
        let memory_start = self.memory.as_mut_ptr();
        let memory_end = unsafe { memory_start.add(MEMORY_SIZE) };

        let ret: *mut VMError = unsafe { raw_fn(this, memory_start, memory_end) };

        if ret.is_null() {
            Ok(())
        } else {
            Err(*unsafe { Box::from_raw(ret) })
        }
    }
}

