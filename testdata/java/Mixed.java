public final class Mixed {
    // Interaction (联动) fixture: combines loops, intra-class recursive/iterative calls, ternary
    // `?:` (stack-φ), and int/long arithmetic in single methods.

    public static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    public static int gcd(int a, int b) {
        while (b != 0) {
            int t = b;
            b = a % b;
            a = t;
        }
        return a;
    }

    // loop + recursive call + ternary + long accumulation
    public static long sumFibSigned(int n) {
        long s = 0;
        for (int i = 0; i <= n; i++) {
            int f = fib(i);
            s += (f & 1) == 0 ? f : -f;
        }
        return s;
    }

    // loop + call(gcd) + ternary: Euler's totient by counting coprimes
    public static int totient(int n) {
        int cnt = 0;
        for (int i = 1; i <= n; i++) {
            cnt += gcd(i, n) == 1 ? 1 : 0;
        }
        return cnt;
    }

    // recursion + nested ternary + arithmetic
    public static int collatzLen(int n) {
        return n <= 1 ? 0 : 1 + collatzLen((n & 1) == 0 ? n / 2 : 3 * n + 1);
    }
}
