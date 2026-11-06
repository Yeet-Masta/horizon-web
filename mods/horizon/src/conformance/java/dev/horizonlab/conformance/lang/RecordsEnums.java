package dev.horizonlab.conformance.lang;

import java.util.EnumMap;
import java.util.EnumSet;
import java.util.List;
import java.util.Map;

/** Records, enums, and every switch shape javac compiles: on enums (the $SwitchMap$ array and the
 * arrow form), on strings (hash then equals), and on patterns (SwitchBootstraps). */
public class RecordsEnums {
    record Point(int x, int y) {
        Point {
            if (x < 0) {
                throw new IllegalArgumentException("negative x " + x);
            }
        }

        Point(int both) {
            this(both, both);
        }

        static Point origin() {
            return new Point(0, 0);
        }

        double length() {
            return Math.sqrt(x * x + y * y);
        }
    }

    record Named<T>(String name, T value) implements Comparable<Named<T>> {
        @Override
        public int compareTo(Named<T> other) {
            return name.compareTo(other.name);
        }
    }

    record Mixed(byte b, short s, char c, long l, float f, double d, boolean z, String str, int[] arr) {}

    sealed interface Shape permits Circle, Square, Rect {}

    record Circle(double r) implements Shape {}

    record Square(double side) implements Shape {}

    record Rect(double w, double h) implements Shape {}

    enum Planet {
        MERCURY(3.303e+23, 2.4397e6),
        EARTH(5.976e+24, 6.37814e6),
        JUPITER(1.9e+27, 7.1492e7);

        private final double mass;
        private final double radius;

        Planet(double mass, double radius) {
            this.mass = mass;
            this.radius = radius;
        }

        double surfaceGravity() {
            return 6.67300E-11 * mass / (radius * radius);
        }
    }

    enum Op {
        PLUS("+") {
            int apply(int a, int b) {
                return a + b;
            }
        },
        TIMES("*") {
            int apply(int a, int b) {
                return a * b;
            }
        };

        final String symbol;

        Op(String symbol) {
            this.symbol = symbol;
        }

        abstract int apply(int a, int b);
    }

    enum Day { MON, TUE, WED, THU, FRI, SAT, SUN }

    static String classic(Day d) {
        switch (d) {
            case SAT:
            case SUN:
                return "weekend";
            case FRI:
                return "almost";
            default:
                return "work";
        }
    }

    static int arrow(Day d) {
        return switch (d) {
            case MON, TUE -> 1;
            case WED -> {
                int x = 2;
                yield x * 10;
            }
            default -> d.ordinal();
        };
    }

    static String strings(String s) {
        switch (s) {
            case "Aa":
                return "Aa";
            case "BB": // same hashCode as "Aa"
                return "BB";
            case "":
                return "empty";
            case "hello":
            case "world":
                return "greeting";
            default:
                return "other:" + s;
        }
    }

    static String patterns(Object o) {
        return switch (o) {
            case null -> "null";
            case Integer i when i > 100 -> "big int " + i;
            case Integer i -> "int " + i;
            case Long l -> "long " + l;
            case String s when s.isEmpty() -> "empty string";
            case String s -> "string " + s.length();
            case Point(int x, int y) when x == y -> "diagonal " + x;
            case Point(var x, var y) -> "point " + x + "," + y;
            case int[] arr -> "ints " + arr.length;
            case Day d -> "day " + d.name().toLowerCase();
            default -> "other " + o.getClass().getSimpleName();
        };
    }

    static double area(Shape s) {
        return switch (s) {
            case Circle c -> Math.PI * c.r() * c.r();
            case Square sq -> sq.side() * sq.side();
            case Rect(double w, double h) -> w * h;
        };
    }

    static String enumPatterns(Day d) {
        return switch (d) {
            case Day x when x.ordinal() >= 5 -> "weekend " + x;
            case MON -> "monday";
            default -> "weekday";
        };
    }

    public static void main(String[] args) {
        Point p = new Point(3, 4);
        System.out.println(p + " " + p.x() + " " + p.length() + " " + new Point(7) + " " + Point.origin());
        System.out.println("equals " + p.equals(new Point(3, 4)) + " " + p.equals(new Point(4, 3)) + " hash "
                + p.hashCode() + " " + new Point(3, 4).hashCode() + " " + Point.origin().hashCode());
        try {
            new Point(-1, 0);
        } catch (IllegalArgumentException e) {
            System.out.println("compact ctor: " + e.getMessage());
        }
        Named<List<String>> named = new Named<>("list", List.of("a", "b"));
        System.out.println(named + " " + named.hashCode() + " cmp " + named.compareTo(new Named<>("m", List.of())));
        Mixed m = new Mixed((byte) -1, (short) 300, 'q', 1L << 40, 1.5f, -0.0, true, null, null);
        System.out.println(m.toString().replaceAll("\\[I@[0-9a-f]+", "[I@") + " " + m.equals(m));
        Mixed m2 = new Mixed((byte) -1, (short) 300, 'q', 1L << 40, 1.5f, -0.0, true, null, null);
        System.out.println("mixed equal " + m.equals(m2) + " hash equal " + (m.hashCode() == m2.hashCode()));
        System.out.println("NaN record " + new Square(Double.NaN).equals(new Square(Double.NaN))
                + " zero " + new Square(0.0).equals(new Square(-0.0)));

        for (Planet planet : Planet.values()) {
            System.out.printf("%s %.2f ordinal=%d%n", planet, planet.surfaceGravity(), planet.ordinal());
        }
        System.out.println(Planet.valueOf("EARTH").compareTo(Planet.JUPITER) + " " + Planet.EARTH.name()
                + " " + Planet.EARTH.getDeclaringClass().getSimpleName());
        try {
            Planet.valueOf("PLUTO");
        } catch (IllegalArgumentException e) {
            System.out.println(e.getMessage());
        }
        for (Op op : Op.values()) {
            System.out.println(op + " " + op.symbol + " " + op.apply(6, 7) + " " + op.getClass().getSuperclass().getSimpleName()
                    + " " + op.getDeclaringClass().getSimpleName());
        }
        for (Day d : Day.values()) {
            System.out.println(d + " " + classic(d) + " " + arrow(d) + " " + enumPatterns(d));
        }
        for (String s : new String[] {"Aa", "BB", "", "hello", "world", "Ab"}) {
            System.out.println("switch '" + s + "' -> " + strings(s));
        }
        for (Object o : new Object[] {null, 5, 500, 7L, "", "four", new Point(2, 2), new Point(1, 2), new int[3],
                Day.FRI, 2.5}) {
            System.out.println(patterns(o));
        }
        for (Shape s : new Shape[] {new Circle(1), new Square(2), new Rect(2, 3)}) {
            System.out.printf("%s %.4f%n", s, area(s));
        }

        EnumMap<Day, Integer> lengths = new EnumMap<>(Day.class);
        lengths.put(Day.SUN, 3);
        lengths.put(Day.MON, 1);
        lengths.put(Day.WED, 2);
        System.out.println(lengths + " " + lengths.containsKey(Day.TUE) + " " + lengths.keySet().iterator().next());
        EnumSet<Day> weekend = EnumSet.of(Day.SUN, Day.SAT);
        System.out.println(weekend + " " + EnumSet.complementOf(weekend) + " " + EnumSet.range(Day.TUE, Day.THU)
                + " " + EnumSet.allOf(Day.class).size() + " " + EnumSet.noneOf(Day.class));
        Map<String, Day> byName = new java.util.TreeMap<>();
        for (Day d : Day.values()) {
            byName.put(d.name().substring(0, 2), d);
        }
        System.out.println(byName);
        Object obj = p;
        if (obj instanceof Point(int x, int y) && x < y) {
            System.out.println("instanceof record pattern " + x + " " + y);
        }
        if (obj instanceof Point q) {
            System.out.println("instanceof binding " + q.y());
        }
    }
}
