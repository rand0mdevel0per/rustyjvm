public final class Square extends Shape {
    Square(int side) {
        super(side); // a real superclass constructor with field writes
    }

    @Override
    int area() {
        return size * size;
    }
}
