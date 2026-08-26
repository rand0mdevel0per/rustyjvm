public class Shape {
    // Increment-6 fixtures: a real class hierarchy — superclass constructors, virtual dispatch
    // through an overridden method, cross-class static calls, and cross-class field access.
    protected int size;

    Shape(int size) {
        this.size = size;
    }

    // Overridden by both subclasses: the call site must dispatch on the runtime class.
    int area() {
        return 0;
    }

    int doubled() {
        return area() * 2; // virtual call on `this` from a superclass method
    }

    static int describe(Shape s) {
        return s.area() + s.size;
    }
}
