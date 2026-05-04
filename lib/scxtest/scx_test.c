// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

#include <stdio.h>

#include "scx_test.h"

__thread jmp_buf scxtest_bail_jmp;

void __fail_assert(const char *condition, const char *file, int line)
{
	fprintf(stderr, "Assertion failed: %s, file %s, line %d\n", condition, file, line);
	longjmp(scxtest_bail_jmp, -1);
}
