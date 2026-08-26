public final class Escape {
    // Increment-5 fixtures: escape analysis (S1 vs S2) and the reference-count fast path.
    int v;
    Escape next;

    // Never escapes: allocated, mutated, and read entirely within this scope -> S1 (zero GC cost).
    public static int local(int a) {
        Escape e = new Escape();
        e.v = a * 2;
        return e.v + 1;
    }

    // Returned, so it outlives its allocating scope -> S2 (reference counted).
    static Escape make(int a) {
        Escape e = new Escape();
        e.v = a;
        return e;
    }

    public static int useReturned(int a) {
        Escape e = make(a);
        return e.v * 3;
    }

    // Stored into another object's field, so the stored object escapes into it -> S2.
    public static int stored(int a, int b) {
        Escape head = new Escape();
        Escape tail = new Escape();
        tail.v = b;
        head.v = a;
        head.next = tail;
        return head.v + head.next.v;
    }

    // A chain built in a loop: each link is stored into the previous one's field.
    public static int chain(int n) {
        Escape head = new Escape();
        head.v = 0;
        Escape cur = head;
        for (int i = 1; i <= n; i++) {
            Escape link = new Escape();
            link.v = i;
            cur.next = link;
            cur = link;
        }
        int sum = 0;
        Escape p = head;
        while (p != null) {
            sum += p.v;
            p = p.next;
        }
        return sum;
    }

    // Allocates many short-lived objects that never escape; exercises S1 RAII reclamation.
    public static int churn(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            Escape e = new Escape();
            e.v = i;
            acc += e.v;
        }
        return acc;
    }
}
