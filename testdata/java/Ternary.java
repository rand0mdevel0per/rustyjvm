public final class Ternary {
    // Stack-φ stress: ternary `?:` (including nested) leaves a value on the operand stack at the
    // merge point, requiring φ over stack slots. All int-in/int-out for a uniform oracle.

    public static int max(int a, int b) {
        return a >= b ? a : b;
    }

    public static int min(int a, int b) {
        return a <= b ? a : b;
    }

    public static int abs(int n) {
        return n < 0 ? -n : n;
    }

    public static int sign(int n) {
        return n < 0 ? -1 : (n > 0 ? 1 : 0);
    }

    public static int clamp(int x, int lo, int hi) {
        return x < lo ? lo : (x > hi ? hi : x);
    }

    public static int med3(int a, int b, int c) {
        return a > b ? (b > c ? b : (a > c ? c : a)) : (a > c ? a : (b > c ? c : b));
    }

    public static int select(int flag, int a, int b) {
        return flag != 0 ? a * 2 + 1 : b * 3 - 1;
    }
}
