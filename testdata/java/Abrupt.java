public final class Abrupt {
    // Regression fixture for a review finding: a frame that leaves abruptly must release what it
    // owned, exactly as a returning frame does. Each call allocates a local (S1, RAII) object and
    // an escaping (S2, counted) one, then fails with a null receiver — so on a bounded heap the
    // same call must be repeatable rather than exhausting it.
    int v;
    Abrupt next;

    static Abrupt escaping(int a) {
        Abrupt e = new Abrupt();
        e.v = a;
        return e;
    }

    public static int throwsAfterAllocating(int a) {
        Abrupt local = new Abrupt(); // S1: reclaimed by RAII at scope exit
        local.v = a;
        Abrupt shared = escaping(a); // S2: reference counted
        Abrupt none = null;
        return local.v + shared.v + none.v; // NullPointerException
    }
}
