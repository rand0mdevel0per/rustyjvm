public final class Slice {
    // Increment-1 vertical slice (RJVM-SPEC-001 plan §"纵向切片规格"): int/long/float
    // arithmetic + conversions + 3-way compares + short-circuit branches. No heap, no invoke.
    // The differential harness constrains b != -1 so `a / (b + 1)` never divides by zero
    // (integer-div traps arrive in increment 8).
    public static int arith(int a, int b, long c, float d) {
        int s = a + b * 3 - (a / (b + 1));
        long t = c * 2L + (long) s;
        float u = d * 1.5f + s;
        if (t > 100L && u < 1000.0f) {
            return (int) (t - (long) u) + s;
        } else if (s % 2 == 0) {
            return s * s;
        } else {
            return -s;
        }
    }
}
