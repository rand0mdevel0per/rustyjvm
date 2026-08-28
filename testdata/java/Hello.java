public final class Hello {
    // Increment-7 fixtures: interned String literals, String methods, and System.out — every
    // observable effect is stdout, compared byte-for-byte against Corretto.

    public static void greet() {
        System.out.println("Hello, RustyJVM!");
        System.out.println("");
        System.out.println("tabs\tand\\backslashes");
    }

    public static void strings() {
        String a = "hello";
        String b = "world";
        System.out.println(a.length());
        System.out.println(b.length());
        System.out.println(a.concat(b));
        System.out.println(a.concat(b).length());
        System.out.println(a.charAt(0));
        System.out.println(a.charAt(4));
        System.out.println(a.hashCode());
        System.out.println(b.hashCode());
        System.out.println("".hashCode());
        System.out.println(a.isEmpty());
        System.out.println("".isEmpty());
        System.out.println(a.equals(b));
        System.out.println(a.equals("hello"));
    }

    // Literal interning is observable: the same literal must be the *same* reference (JLS 3.10.5).
    public static void interning() {
        String x = "shared";
        String y = "shared";
        String z = "other";
        System.out.println(x == y);
        System.out.println(x == z);
        System.out.println(x.equals(y));
    }

    public static void primitives() {
        System.out.println(42);
        System.out.println(-7);
        System.out.println(2147483647);
        System.out.println(-2147483648);
        System.out.println(1234567890123L);
        System.out.println(true);
        System.out.println(false);
        System.out.print("no");
        System.out.print("newline");
        System.out.println("");
    }

    public static void unicode() {
        String s = "héllo 中文";
        System.out.println(s);
        System.out.println(s.length());
        System.out.println(s.hashCode());
    }

    public static void main(String[] args) {
        greet();
        strings();
        interning();
        primitives();
        unicode();
    }
}
