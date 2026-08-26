public final class Shapes {
    // Increment-6 entry points: every method returns int so one oracle shape covers them all.
    // Exercises cross-class `new`, superclass constructors, virtual dispatch, cross-class static
    // calls, cross-class field reads, `instanceof`/`checkcast`, and static initialisers.

    static int counter;          // static field
    static final int BASE;       // set by <clinit>

    static {
        BASE = 1000;
        counter = 7;
    }

    public static int squareArea(int side) {
        Shape s = new Square(side);
        return s.area(); // virtual: resolves to Square.area
    }

    public static int rectArea(int w, int h) {
        Shape s = new Rect(w, h);
        return s.area(); // virtual: resolves to Rect.area
    }

    // The same call site sees two runtime classes -> the dispatch table must be consulted, not baked.
    public static int polymorphic(int a, int b) {
        Shape x = new Square(a);
        Shape y = new Rect(a, b);
        return x.area() + y.area() + x.doubled() + y.doubled();
    }

    // Cross-class static call + cross-class (inherited, protected) field read.
    public static int described(int a, int b) {
        return Shape.describe(new Square(a)) + Shape.describe(new Rect(a, b));
    }

    // A superclass constructor's field write must actually happen.
    public static int superCtor(int side) {
        Square sq = new Square(side);
        return sq.size; // written by Shape(int), not by Square
    }

    public static int instanceOf(int a) {
        Shape s = new Square(a);
        int r = 0;
        if (s instanceof Square) {
            r += 1;
        }
        if (s instanceof Rect) {
            r += 2;
        }
        Square back = (Square) s; // checkcast
        return r * 100 + back.area();
    }

    // Static fields and <clinit>.
    public static int statics(int a) {
        counter += a;
        return BASE + counter;
    }
}
