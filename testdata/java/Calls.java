public final class Calls {
    // Increment-2b + stack-φ: intra-class static calls with native ternary `?:` (which leaves a
    // value on the operand stack at the merge — exercised via φ over stack slots), recursion,
    // mutual recursion, and an unbounded recursion for the StackOverflowError seam.

    public static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    public static long fact(int n) {
        return n <= 1 ? 1L : (long) n * fact(n - 1);
    }

    public static int isEven(int n) {
        return n == 0 ? 1 : isOdd(n - 1);
    }

    public static int isOdd(int n) {
        return n == 0 ? 0 : isEven(n - 1);
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
