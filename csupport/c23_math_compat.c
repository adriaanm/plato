/* c23_math_compat.c -- the four C23 libm functions musl does not have.
 *
 * ezkindle fork, phase 1.  This is not a Plato problem and not a zig problem:
 * rustc 1.97 lowers `f32::min` / `f32::max` (and the f64 pair) to the C23
 * library functions fminimum_num* / fmaximum_num*.  glibc has had them since
 * 2.35; **musl 1.2 does not**, so every Rust binary containing a float min or
 * max fails to link for any *-linux-musl* target with
 *
 *     ld.lld: error: undefined symbol: fminimum_numf
 *     >>> referenced by ... paragraph_breaker::total_fit ...
 *
 * Four functions, exact C23 semantics (N3096 7.12.12.4/5):
 *   - if exactly one argument is a NaN, return the other one (this is what
 *     makes them the "_num" variants rather than fminimum/fmaximum);
 *   - if both are NaNs, return a NaN;
 *   - -0.0 compares less than +0.0, which `<` does not do.
 *
 * Deliberately no fast paths: this is called once per line break, and being
 * obviously correct is worth more than being clever.
 */

#include <math.h>

float fminimum_numf(float x, float y)
{
	if (isnan(x)) return y;
	if (isnan(y)) return x;
	if (x < y) return x;
	if (y < x) return y;
	/* Equal, so the only remaining question is the sign of zero. */
	return signbit(x) ? x : y;
}

float fmaximum_numf(float x, float y)
{
	if (isnan(x)) return y;
	if (isnan(y)) return x;
	if (x > y) return x;
	if (y > x) return y;
	return signbit(x) ? y : x;
}

double fminimum_num(double x, double y)
{
	if (isnan(x)) return y;
	if (isnan(y)) return x;
	if (x < y) return x;
	if (y < x) return y;
	return signbit(x) ? x : y;
}

double fmaximum_num(double x, double y)
{
	if (isnan(x)) return y;
	if (isnan(y)) return x;
	if (x > y) return x;
	if (y > x) return y;
	return signbit(x) ? y : x;
}
