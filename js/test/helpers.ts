import { createHash } from "node:crypto";

export function md5(data: Uint8Array | string): string {
  return createHash("md5").update(data).digest("hex");
}

export function pattern(n: number): Uint8Array {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i++) b[i] = (i * 31 + Math.floor(i / 7)) & 0xff;
  return b;
}

export async function median(n: number, f: () => Promise<unknown>): Promise<number> {
  const samples: number[] = [];
  for (let i = 0; i < n; i++) {
    const start = performance.now();
    await f();
    samples.push(performance.now() - start);
  }
  samples.sort((a, b) => a - b);
  return samples[Math.floor(n / 2)]!;
}

export function micros(ms: number): string {
  return `${(ms * 1000).toFixed(0)}us`;
}
