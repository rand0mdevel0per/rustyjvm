public final class Calls {
    // Increment-2b fixtures: intra-class static calls, recursion, mutual recursion, and an
    // unbounded recursion for the StackOverflowError seam. Written in if-return style so operand
    // stacks are empty at basic-block boundaries (ternary `?:` leaves a value on the stack at the
    // merge — that non-empty-stack SSA case is a separate IR enhancement, added when needed).

    public static int fib(int n) {
        if (n < 2) {
            return n;
        }
        return fib(n - 1) + fib(n - 2);
    }

    public static long fact(int n) {
        if (n <= 1) {
            return 1L;
        }
        return (long) n * fact(n - 1);
    }

    public static int isEven(int n) {
        if (n == 0) {
            return 1;
        }
        return isOdd(n - 1);
    }

    public static int isOdd(int n) {
        if (n == 0) {
            return 0;
        }
        return isEven(n - 1);
    }

    public static int add(int a, int b) {
        return a + b;
    }

    public static int addAll(int a, int b, int c) {
        return add(add(a, b), c);
    }

    public static int deep(int n) {
        return deep(n + 1) + 1; // never returns; drives StackOverflowError
    }
}
