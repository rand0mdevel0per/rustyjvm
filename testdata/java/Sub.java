public class Sub extends SubBase {
    // Regression fixture for a review finding: a real superclass constructor has observable field
    // writes, so its cross-class `invokespecial` must NOT be treated as a no-op. Until the loader
    // lands (increment 6), building this must fail loudly rather than silently mis-initialise.
    int extra;

    Sub() {
        super(7);
        this.extra = 1;
    }

    public static int viaSuper() {
        Sub s = new Sub();
        return s.base + s.extra; // Corretto: 7 + 1 == 8
    }
}

class SubBase {
    int base;

    SubBase(int v) {
        this.base = v;
    }
}
