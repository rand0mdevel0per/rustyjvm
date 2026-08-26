public final class Point {
    // Increment-3 object fixtures: `new`, getfield/putfield, invokespecial (<init>), local S1
    // objects. All static methods return int for a uniform differential oracle.
    int x;
    int y;

    // local object, set then read fields
    public static int normSq(int a, int b) {
        Point p = new Point();
        p.x = a;
        p.y = b;
        return p.x * p.x + p.y * p.y;
    }

    // two local objects + ternary (Manhattan distance)
    public static int manhattan(int ax, int ay, int bx, int by) {
        Point a = new Point();
        a.x = ax;
        a.y = ay;
        Point b = new Point();
        b.x = bx;
        b.y = by;
        int dx = a.x - b.x;
        int dy = a.y - b.y;
        return (dx < 0 ? -dx : dx) + (dy < 0 ? -dy : dy);
    }

    // field default values are zero (JVMS object allocation zeroes fields)
    public static int defaults() {
        Point p = new Point();
        return p.x + p.y;
    }

    // object in a loop: accumulate via fields
    public static int accumulate(int n) {
        Point acc = new Point();
        for (int i = 1; i <= n; i++) {
            acc.x = acc.x + i;
            acc.y = acc.y + acc.x;
        }
        return acc.x + acc.y;
    }
}
