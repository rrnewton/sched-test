/*
 * sim_sigfpe.c - SIGFPE handler giving BPF division/modulo semantics
 *
 * scxsim compiles BPF scheduler source as ordinary userspace C. Native x86-64
 * raises #DE (SIGFPE) for cases that BPF defines as producing a value, so this
 * handler decodes the faulting div/idiv, applies the BPF-defined result, and
 * resumes after the instruction.
 *
 * The semantics implemented here are the ones the kernel verifier itself
 * inserts as fixup patchlets around every runtime-variable divisor
 * (kernel/bpf/verifier.c, fixup_bpf_calls / chk_and_* patchlets):
 *
 *     chk_and_div:   [R,W]x div  0  -> 0
 *     chk_and_mod:   [R,W]x mod  0  -> [R,W]x        (dividend, NOT zero)
 *     chk_and_sdiv:  [R,W]x sdiv 0  -> 0
 *                    LLONG_MIN sdiv -1 -> LLONG_MIN
 *                    INT_MIN   sdiv -1 -> INT_MIN
 *     chk_and_smod:  [R,W]x smod 0  -> [R,W]x
 *                    [R,W]x smod -1 -> 0
 *
 * A CONSTANT zero divisor is a different case: the verifier rejects the
 * program outright ("div by zero", verifier.c check_alu_op). Such code can
 * never run on a real kernel, so it is not modelled here — see
 * ai_docs/BPF_UB_FIDELITY_POLICY.md.
 *
 * x86-64 writes the quotient to (R|E)AX and the remainder to (R|E)DX, and the
 * compiler then reads whichever one the source asked for. We therefore set
 * BOTH to their BPF-defined values, which satisfies the div rule and the mod
 * rule simultaneously without having to know which the source wanted.
 *
 * HISTORY: this handler previously set RAX=0 and RDX=0 unconditionally. That
 * is right for division but WRONG for modulo (BPF yields the dividend, not 0)
 * and wrong for signed overflow (BPF yields the dividend, not 0), so `x % 0`
 * and `INT_MIN / -1` inside a simulated scheduler silently produced values the
 * kernel would never produce.
 *
 * This file does NOT include sim_wrapper.h or vmlinux.h to avoid
 * conflicts with <signal.h> type definitions.
 */

#define _GNU_SOURCE
#include <signal.h>
#include <ucontext.h>
#include <string.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "sim_sigfpe.c implements BPF division semantics by decoding x86-64 div/idiv. Port the decoder before building scxsim on this architecture; a missing handler would silently give native C semantics where BPF defines a value."
#endif

/*
 * Async-signal-safe fatal exit. Per the No Silent Failures rule, an
 * undecodable fault must abort loudly rather than resume with a guessed
 * value that the kernel would never produce.
 */
static void sim_sigfpe_fatal(const char *msg)
{
	ssize_t ignored;

	ignored = write(2, msg, strlen(msg));
	(void)ignored;
	_exit(70);
}

/*
 * Map an x86-64 register encoding (0-15: RAX,RCX,RDX,RBX,RSP,RBP,RSI,RDI,
 * R8..R15) to its glibc mcontext gregs index. Returns -1 if out of range.
 */
static int sim_greg_index(unsigned int x86_reg)
{
	static const int map[16] = {
		REG_RAX, REG_RCX, REG_RDX, REG_RBX,
		REG_RSP, REG_RBP, REG_RSI, REG_RDI,
		REG_R8,  REG_R9,  REG_R10, REG_R11,
		REG_R12, REG_R13, REG_R14, REG_R15,
	};

	if (x86_reg >= 16)
		return -1;
	return map[x86_reg];
}

/*
 * Compute the effective address of a memory r/m operand.
 *
 * `insn_end` is the offset of the byte AFTER the whole instruction, which
 * RIP-relative addressing is measured from. `sib` is meaningful only when
 * rm == 4; `disp` was already consumed by the caller.
 */
static const void *sim_decode_ea(const mcontext_t *mc, const unsigned char *rip,
				 int insn_end, unsigned char rex,
				 unsigned char mod, unsigned char rm,
				 unsigned char sib, int disp)
{
	unsigned long addr = 0;
	int gi;

	if (mod == 0 && rm == 5)
		return (const void *)(rip + insn_end + disp); /* RIP-relative */

	if (rm == 4) {
		unsigned int base = (sib & 7) | ((rex & 0x01) << 3);
		unsigned int index = ((sib >> 3) & 7) | ((rex & 0x02) << 2);
		unsigned int scale = 1u << ((sib >> 6) & 3);

		/* index==4 with no REX.X means "no index register". */
		if (index != 4) {
			gi = sim_greg_index(index);
			if (gi < 0)
				sim_sigfpe_fatal("scxsim: SIGFPE with an "
						 "undecodable SIB index\n");
			addr += (unsigned long)mc->gregs[gi] * scale;
		}

		/* base==5 with mod==0 means "no base", disp32 only. */
		if (!(mod == 0 && (sib & 7) == 5)) {
			gi = sim_greg_index(base);
			if (gi < 0)
				sim_sigfpe_fatal("scxsim: SIGFPE with an "
						 "undecodable SIB base\n");
			addr += (unsigned long)mc->gregs[gi];
		}
	} else {
		gi = sim_greg_index(rm | ((rex & 0x01) << 3));
		if (gi < 0)
			sim_sigfpe_fatal("scxsim: SIGFPE with an undecodable "
					 "base register\n");
		addr += (unsigned long)mc->gregs[gi];
	}

	return (const void *)(addr + (unsigned long)(long)disp);
}

static void sim_sigfpe_handler(int sig, siginfo_t *info, void *ctx)
{
	ucontext_t *uc = (ucontext_t *)ctx;
	mcontext_t *mc = &uc->uc_mcontext;
	unsigned char *rip = (unsigned char *)mc->gregs[REG_RIP];
	unsigned char rex = 0, modrm, mod, rm, reg, sib = 0;
	int off = 0, is64, is_signed, divisor_is_minus_one = 0, disp = 0;
	greg_t dividend;

	(void)sig;
	(void)info;

	/*
	 * Operand-size and address-size prefixes would change the decode below.
	 * BPF has only 32-bit and 64-bit division, so clang never emits them
	 * here; refuse rather than mis-decode.
	 */
	if (rip[off] == 0x66 || rip[off] == 0x67)
		sim_sigfpe_fatal("scxsim: SIGFPE on div/idiv with an operand-size "
				 "prefix; decoder supports only 32/64-bit forms\n");

	/* REX prefix (0x40-0x4F): W selects 64-bit operands, B extends r/m. */
	if ((rip[off] & 0xf0) == 0x40)
		rex = rip[off++];
	is64 = (rex & 0x08) != 0;

	/*
	 * F7 /6 = div r/m{32,64}, F7 /7 = idiv r/m{32,64}.
	 * F6 is the 8-bit form; BPF has no 8-bit division, so it should never
	 * appear and is not decoded (AL/AH result layout differs).
	 */
	if (rip[off] != 0xF7)
		sim_sigfpe_fatal("scxsim: SIGFPE at a non-F7 opcode; expected "
				 "32/64-bit div or idiv\n");
	off++;

	modrm = rip[off++];
	mod = (modrm >> 6) & 3;
	reg = (modrm >> 3) & 7;
	rm = modrm & 7;

	if (reg != 6 && reg != 7)
		sim_sigfpe_fatal("scxsim: SIGFPE on an F7 form that is not "
				 "div (/6) or idiv (/7)\n");
	is_signed = (reg == 7);

	/*
	 * Consume SIB / displacement bytes. This both finds the end of the
	 * instruction (needed to resume, and to resolve RIP-relative operands)
	 * and captures the pieces the effective-address computation needs.
	 */
	if (mod != 3) {
		if (rm == 4)
			sib = rip[off++];

		if (mod == 1) {
			disp = (int)(signed char)rip[off++];
		} else if (mod == 2) {
			memcpy(&disp, rip + off, 4);
			off += 4;
		} else if (rm == 5 || (rm == 4 && (sib & 7) == 5)) {
			/* mod==0: disp32 with RIP-relative or no-base SIB */
			memcpy(&disp, rip + off, 4);
			off += 4;
		}
	}
	/* mod == 3: register-direct, no extra bytes */

	/*
	 * Determine WHY we faulted, because divisor==0 and signed overflow
	 * (dividend==INT_MIN/LLONG_MIN with divisor==-1) have DIFFERENT
	 * BPF-defined results. Both forms occur in practice: clang keeps a
	 * divisor in a register when it can, but divides straight out of
	 * memory for globals and struct fields.
	 */
	{
		greg_t divisor;

		if (mod == 3) {
			unsigned int x86_reg = rm | ((rex & 0x01) << 3);
			int gi = sim_greg_index(x86_reg);

			if (gi < 0)
				sim_sigfpe_fatal("scxsim: SIGFPE with an "
						 "undecodable divisor register\n");
			divisor = mc->gregs[gi];
		} else {
			const void *addr = sim_decode_ea(mc, rip, off, rex,
							 mod, rm, sib, disp);

			/*
			 * The faulting instruction already read this operand
			 * successfully, so the address is mapped.
			 */
			if (is64)
				divisor = (greg_t)*(const long long *)addr;
			else
				divisor = (greg_t)*(const int *)addr;
		}

		if (!is64)
			divisor = (greg_t)(int)(unsigned int)divisor;
		divisor_is_minus_one = is_signed && divisor == (greg_t)-1;
	}

	/* The dividend is in (R|E)AX on entry to div/idiv. */
	dividend = mc->gregs[REG_RAX];
	if (!is64)
		dividend = (greg_t)(unsigned int)dividend;

	if (divisor_is_minus_one) {
		/* LLONG_MIN sdiv -1 -> LLONG_MIN ; x smod -1 -> 0 */
		mc->gregs[REG_RAX] = dividend;
		mc->gregs[REG_RDX] = 0;
	} else {
		/* x div 0 -> 0 ; x mod 0 -> x */
		mc->gregs[REG_RAX] = 0;
		mc->gregs[REG_RDX] = dividend;
	}

	mc->gregs[REG_RIP] = (greg_t)(rip + off);
}

void sim_install_sigfpe_handler(void)
{
	struct sigaction sa;

	memset(&sa, 0, sizeof(sa));
	sa.sa_sigaction = sim_sigfpe_handler;
	sa.sa_flags = SA_SIGINFO;
	sigemptyset(&sa.sa_mask);
	sigaction(SIGFPE, &sa, NULL);
}
