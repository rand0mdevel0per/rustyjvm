public final class Rect extends Shape {
    private final int height;

    Rect(int w, int h) {
        super(w);
        this.height = h;
    }

    @Override
    int area() {
        return size * height;
    }
}
