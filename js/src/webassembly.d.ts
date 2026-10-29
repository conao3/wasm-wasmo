declare namespace WebAssembly {
  class Module {
    constructor(bytes: BufferSource);
  }
  class Instance {
    constructor(module: Module, imports?: Record<string, Record<string, unknown>>);
    readonly exports: Record<string, unknown>;
  }
  class Memory {
    readonly buffer: ArrayBuffer;
  }
  function compile(bytes: BufferSource): Promise<Module>;
}
