package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.function.BiFunction;
import java.util.function.BinaryOperator;
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.IntFunction;
import java.util.function.Predicate;
import java.util.function.Supplier;
import java.util.function.ToIntFunction;
import java.util.function.UnaryOperator;

/** Lambdas and method references: captures, every kind of reference, the functional interfaces'
 * default methods, and Comparator chains. */
public class Lambdas {
    interface Shape {
        double area();

        default String describe() {
            return getClass().getSimpleName().isEmpty() ? "lambda" : "shape " + Math.round(area());
        }
    }

    @FunctionalInterface
    interface TriFunction<A, B, C, R> {
        R apply(A a, B b, C c);

        default <V> TriFunction<A, B, C, V> andThen(Function<? super R, ? extends V> after) {
            return (a, b, c) -> after.apply(apply(a, b, c));
        }
    }

    record Person(String name, int age) {}

    private final String prefix;
    private int counter;
    static Function<Integer, Integer> factorial;

    static {
        factorial = n -> n <= 1 ? 1 : n * factorial.apply(n - 1);
    }

    Lambdas(String prefix) {
        this.prefix = prefix;
    }

    Supplier<String> greeter(String name) {
        return () -> prefix + ", " + name + " #" + (++counter);
    }

    static int twice(int x) {
        return x * 2;
    }

    String tag(String s) {
        return prefix + ":" + s;
    }

    public static void main(String[] args) {
        int base = 10;
        Function<Integer, Integer> addBase = x -> x + base;
        UnaryOperator<Integer> square = x -> x * x;
        System.out.println("compose " + addBase.compose(square).apply(3));
        System.out.println("andThen " + addBase.andThen(square).apply(3));
        System.out.println("identity " + Function.<String>identity().apply("same"));

        Lambdas outer = new Lambdas("Hello");
        Supplier<String> hi = outer.greeter("Ada");
        System.out.println(hi.get());
        System.out.println(hi.get());
        System.out.println("counter " + outer.counter);

        // Every kind of method reference.
        Function<Integer, Integer> staticRef = Lambdas::twice;
        Function<String, String> boundRef = outer::tag;
        Function<String, Integer> unboundRef = String::length;
        BiFunction<String, String, Boolean> unboundTwo = String::startsWith;
        Supplier<ArrayList<String>> ctorRef = ArrayList::new;
        Function<String, StringBuilder> ctorArg = StringBuilder::new;
        IntFunction<int[]> arrayCtor = int[]::new;
        Function<Integer, String[]> boxedArrayCtor = String[]::new;
        System.out.println(staticRef.apply(21) + " " + boundRef.apply("x") + " " + unboundRef.apply("four")
                + " " + unboundTwo.apply("prefix", "pre"));
        ArrayList<String> list = ctorRef.get();
        list.add("a");
        System.out.println(list + " " + ctorArg.apply("sb").reverse() + " " + arrayCtor.apply(3).length
                + " " + boxedArrayCtor.apply(2).length);

        Predicate<String> empty = String::isEmpty;
        Predicate<String> longWord = s -> s.length() > 4;
        for (String s : new String[] {"", "tiny", "enormous"}) {
            System.out.println("'" + s + "' empty=" + empty.test(s) + " notEmpty=" + empty.negate().test(s)
                    + " emptyOrLong=" + empty.or(longWord).test(s) + " notEmptyAndLong="
                    + empty.negate().and(longWord).test(s) + " isEqual=" + Predicate.isEqual("tiny").test(s)
                    + " not=" + Predicate.not(longWord).test(s));
        }

        BinaryOperator<Integer> max = BinaryOperator.maxBy(Comparator.naturalOrder());
        BinaryOperator<Integer> min = BinaryOperator.minBy(Comparator.naturalOrder());
        System.out.println("maxBy " + max.apply(3, 9) + " minBy " + min.apply(3, 9));
        IntBinaryOperator gcd = new IntBinaryOperator() {
            @Override
            public int applyAsInt(int a, int b) {
                return b == 0 ? a : applyAsInt(b, a % b);
            }
        };
        System.out.println("gcd " + gcd.applyAsInt(84, 36));
        System.out.println("factorial " + factorial.apply(10));

        TriFunction<Integer, Integer, Integer, Integer> sum3 = (a, b, c) -> a + b + c;
        System.out.println("tri " + sum3.andThen(x -> "=" + x).apply(1, 2, 3));

        Shape unit = () -> 3.14159;
        System.out.println(unit.describe() + " " + unit.area());

        List<Person> people = new ArrayList<>(List.of(new Person("Cy", 30), new Person("Al", 25),
                new Person("Bo", 30), new Person("Di", 25), new Person("Ed", 41)));
        people.sort(Comparator.comparingInt(Person::age).thenComparing(Person::name));
        System.out.println(people);
        people.sort(Comparator.comparing(Person::age, Comparator.reverseOrder()).thenComparing(Person::name,
                Comparator.reverseOrder()));
        System.out.println(people);
        people.sort(Comparator.comparing(Person::name).reversed());
        System.out.println(people);
        ToIntFunction<Person> age = Person::age;
        System.out.println("ages " + people.stream().mapToInt(age).sum());
        List<String> names = Arrays.asList("delta", null, "alpha", "Charlie", null, "bravo");
        names.sort(Comparator.nullsFirst(String.CASE_INSENSITIVE_ORDER));
        System.out.println(names);
        names.sort(Comparator.nullsLast(Comparator.<String>naturalOrder().reversed()));
        System.out.println(names);

        // A lambda captures the value, not the variable: each Runnable sees its own i.
        List<Runnable> tasks = new ArrayList<>();
        for (int i = 0; i < 3; i++) {
            final int copy = i;
            tasks.add(() -> System.out.println("task " + copy));
        }
        tasks.forEach(Runnable::run);
        Runnable r1 = () -> {};
        Runnable r2 = () -> {};
        System.out.println("distinct lambdas " + (r1 != r2) + " same lambda class shape "
                + (r1.getClass().isSynthetic() == r2.getClass().isSynthetic()));
        Object o = (Runnable & java.io.Serializable) () -> {};
        System.out.println("intersection cast serializable " + (o instanceof java.io.Serializable));
    }
}
