use core::arch::global_asm;

// gatekv_crc32c(crc: u32 = edi, p: *const u8 = rsi, n: usize = rdx) -> u32 in eax
//   Raw CRC32C update, without pre- or post-inversion: one SSE4.2 `crc32` per quadword, then
//   one per trailing byte. A single dependency chain, so about a third of the instruction's
//   peak throughput; three interleaved streams would need a CLMUL-based merge.
//   Clobbers only rax, rcx, rdx, rsi and flags. Leaf: no stack use (alignment is moot), no
//   memory writes. Requires SSE4.2 and p readable for n bytes (unread when n == 0).
//
// gatekv_hash(p: *const u8 = rdi, n: usize = rsi) -> u64 in rax
//   With K = 0x9E3779B97F4A7C15: h = n * K, then h = (rol(h, 5) ^ w) * K for each
//   little-endian quadword w and once more for the zero-padded tail, then h ^= h >> 32,
//   h *= K, h ^= h >> 29. The 1..7 tail bytes are gathered one at a time from the end so that
//   nothing past p + n is read, even when the key ends at a page boundary.
//   Clobbers only rax, rcx, rdx, rsi, rdi, r8 and flags. Leaf: no stack use, no memory
//   writes. Requires p readable for n bytes (unread when n == 0).
global_asm!(
    r#"
    .pushsection .text
    .globl gatekv_crc32c
    .type gatekv_crc32c, @function
    .p2align 4
gatekv_crc32c:
    mov eax, edi
    mov rcx, rdx
    shr rcx, 3
    jz .Lcrc_tail
.Lcrc_word:
    crc32 rax, qword ptr [rsi]
    add rsi, 8
    dec rcx
    jnz .Lcrc_word
.Lcrc_tail:
    and edx, 7
    jz .Lcrc_done
.Lcrc_byte:
    crc32 eax, byte ptr [rsi]
    inc rsi
    dec edx
    jnz .Lcrc_byte
.Lcrc_done:
    ret
    .size gatekv_crc32c, . - gatekv_crc32c

    .globl gatekv_hash
    .type gatekv_hash, @function
    .p2align 4
gatekv_hash:
    mov r8, 0x9E3779B97F4A7C15
    mov rax, rsi
    imul rax, r8
    mov rcx, rsi
    shr rcx, 3
    jz .Lhash_tail
.Lhash_word:
    rol rax, 5
    xor rax, qword ptr [rdi]
    imul rax, r8
    add rdi, 8
    dec rcx
    jnz .Lhash_word
.Lhash_tail:
    and esi, 7
    jz .Lhash_mix
    xor edx, edx
.Lhash_byte:
    shl rdx, 8
    movzx ecx, byte ptr [rdi + rsi - 1]
    or rdx, rcx
    dec esi
    jnz .Lhash_byte
    rol rax, 5
    xor rax, rdx
    imul rax, r8
.Lhash_mix:
    mov rdx, rax
    shr rdx, 32
    xor rax, rdx
    imul rax, r8
    mov rdx, rax
    shr rdx, 29
    xor rax, rdx
    ret
    .size gatekv_hash, . - gatekv_hash
    .popsection
"#
);

unsafe extern "C" {
    fn gatekv_crc32c(crc: u32, p: *const u8, n: usize) -> u32;
    fn gatekv_hash(p: *const u8, n: usize) -> u64;
}

/// CRC32C (Castagnoli) of `data`, where `seed` is the CRC of the bytes before it:
/// `crc32c(crc32c(0, a), b) == crc32c(0, a ++ b)`.
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    // SAFETY: the routine reads data[..data.len()] only, writes no memory and leaves every
    // callee-saved register alone. Without SSE4.2 it faults with SIGILL rather than
    // corrupting anything, and main() refuses to start on such a CPU.
    !unsafe { gatekv_crc32c(!seed, data.as_ptr(), data.len()) }
}

pub fn hash(key: &[u8]) -> u64 {
    // SAFETY: the routine reads key[..key.len()] only, writes no memory and leaves every
    // callee-saved register alone.
    unsafe { gatekv_hash(key.as_ptr(), key.len()) }
}
