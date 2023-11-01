#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("IO: {0}")]
    IO(#[from] std::io::Error),
    #[error("Pointer Overflow")]
    PointerOverflow,
}

#[derive(Debug, thiserror::Error)]
pub enum VMError {
    // IO错误
    #[error("IO: {0}")]
    IO(#[from] std::io::Error),
    // 编译错误
    #[error("Compile: {0}")]
    Compile(#[from] crate::bfir::CompileError),
    // 运行时错误
    #[error("Runtime: {0}")]
    Runtime(#[from] RuntimeError),
}

pub type Result<T> = std::result::Result<T, VMError>;