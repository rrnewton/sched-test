/*
 * e9_rbc_trampoline.c — e9patch call trampoline stub.
 *
 * This is compiled with e9compile.sh. It's a minimal stub that forwards
 * to the rbc_trampoline() function in the host .so (from sim_rbc_trampoline.c).
 *
 * The rbc_trampoline symbol is resolved by the dynamic linker since both
 * the injected trampoline code and the host .so share the same address space.
 */

/* Forward declaration — resolved from the host .so's symbol table. */
extern void rbc_trampoline(void);

/* The entry point called by e9tool. Just forwards to the host. */
void entry(void)
{
    rbc_trampoline();
}
