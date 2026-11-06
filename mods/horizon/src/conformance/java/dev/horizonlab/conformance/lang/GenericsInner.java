package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.Collection;
import java.util.Iterator;
import java.util.List;
import java.util.NoSuchElementException;
import java.util.function.Supplier;

/** Generics and bridge methods, covariant returns, inner/local/anonymous classes reaching their
 * outer instance's private state (nestmates), and interfaces with default, static and private
 * methods, including the diamond `A.super.m()` call. */
public class GenericsInner {
    private int secret = 42;
    private static String staticSecret = "static";

    abstract static class Animal implements Comparable<Animal> {
        abstract String sound();

        Animal self() {
            return this;
        }

        @Override
        public int compareTo(Animal o) {
            return sound().compareTo(o.sound());
        }

        @Override
        public String toString() {
            return getClass().getSimpleName() + "(" + sound() + ")";
        }
    }

    static class Dog extends Animal {
        @Override
        String sound() {
            return "woof";
        }

        @Override
        Dog self() { // covariant return: javac adds a bridge returning Animal
            return this;
        }
    }

    static class Cat extends Animal {
        @Override
        String sound() {
            return "meow";
        }
    }

    interface Box<T> {
        T get();

        void set(T value);
    }

    static class IntBox implements Box<Integer> {
        private Integer value = 0;

        @Override
        public Integer get() {
            return value;
        }

        @Override
        public void set(Integer value) { // bridge set(Object) casts to Integer
            this.value = value;
        }
    }

    static <T extends Comparable<? super T>> T max(Collection<? extends T> items) {
        Iterator<? extends T> it = items.iterator();
        T best = it.next();
        while (it.hasNext()) {
            T next = it.next();
            if (next.compareTo(best) > 0) {
                best = next;
            }
        }
        return best;
    }

    @SafeVarargs
    static <T> List<T> listOf(T... items) {
        List<T> out = new ArrayList<>();
        for (T item : items) {
            out.add(item);
        }
        return out;
    }

    static double sum(List<? extends Number> numbers) {
        double total = 0;
        for (Number n : numbers) {
            total += n.doubleValue();
        }
        return total;
    }

    static void fill(List<? super Integer> sink, int n) {
        for (int i = 0; i < n; i++) {
            sink.add(i);
        }
    }

    interface Left {
        default String who() {
            return "left";
        }

        static String id() {
            return "Left.id";
        }
    }

    interface Right {
        default String who() {
            return "right";
        }

        private String hidden() {
            return "private interface method";
        }

        default String reveal() {
            return hidden();
        }
    }

    static class Both implements Left, Right {
        @Override
        public String who() {
            return Left.super.who() + "+" + Right.super.who();
        }
    }

    class Inner {
        int peek() {
            return secret + 1; // reads the outer instance's private field
        }

        class Deeper {
            String path() {
                return "deeper sees " + secret + " and " + GenericsInner.this.secret + " " + staticSecret;
            }
        }
    }

    static class Range implements Iterable<Integer> {
        private final int from;
        private final int to;

        Range(int from, int to) {
            this.from = from;
            this.to = to;
        }

        @Override
        public Iterator<Integer> iterator() {
            return new Iterator<>() {
                private int next = from;

                @Override
                public boolean hasNext() {
                    return next < to;
                }

                @Override
                public Integer next() {
                    if (!hasNext()) {
                        throw new NoSuchElementException();
                    }
                    return next++;
                }
            };
        }
    }

    abstract static class Counter {
        protected int count;

        Counter(int start) {
            count = start;
            init();
        }

        abstract void init();
    }

    public static void main(String[] args) {
        List<Animal> animals = listOf(new Dog(), new Cat(), new Dog());
        System.out.println(animals + " max=" + max(animals) + " self=" + new Dog().self());
        Animal a = new Dog();
        System.out.println("virtual self " + a.self().getClass().getSimpleName());

        Box<Integer> box = new IntBox();
        box.set(7);
        System.out.println("box " + box.get());
        @SuppressWarnings({"unchecked", "rawtypes"})
        Box<Object> raw = (Box) box;
        try {
            raw.set("not an int"); // the bridge's checkcast throws
        } catch (ClassCastException e) {
            System.out.println("bridge checkcast threw " + e.getClass().getSimpleName());
        }
        List<Object> sink = new ArrayList<>();
        fill(sink, 4);
        System.out.println("sum " + sum(listOf(1, 2.5, 3L)) + " sink " + sink);
        List<String> strings = new ArrayList<>();
        @SuppressWarnings({"unchecked", "rawtypes"})
        List<Object> erased = (List) strings;
        erased.add(5);
        try {
            String s = strings.get(0);
            System.out.println("no throw " + s);
        } catch (ClassCastException e) {
            System.out.println("heap pollution caught at use");
        }

        Both both = new Both();
        System.out.println(both.who() + " " + both.reveal() + " " + Left.id());

        GenericsInner outer = new GenericsInner();
        GenericsInner.Inner inner = outer.new Inner();
        GenericsInner.Inner.Deeper deeper = inner.new Deeper();
        outer.secret = 50;
        System.out.println("inner " + inner.peek() + " " + deeper.path());

        int local = 5;
        class LocalAdder {
            int add(int x) {
                return x + local + outer.secret;
            }
        }
        System.out.println("local class " + new LocalAdder().add(1));

        Counter c = new Counter(10) {
            private int extra = 100; // runs after the super constructor's init() call

            {
                count += 1;
            }

            @Override
            void init() {
                count += extra; // extra is still 0 here
            }
        };
        System.out.println("anonymous " + c.count + " " + c.getClass().getSuperclass().getSimpleName()
                + " anonymous=" + c.getClass().isAnonymousClass());

        int total = 0;
        for (int i : new Range(1, 5)) {
            total += i;
        }
        System.out.println("iterable " + total);
        Iterator<Integer> it = new Range(0, 1).iterator();
        it.next();
        try {
            it.next();
        } catch (NoSuchElementException e) {
            System.out.println("exhausted");
        }
        Supplier<Supplier<String>> nested = () -> () -> "nested lambda " + staticSecret;
        System.out.println(nested.get().get());
        System.out.println("nestmates " + Inner.class.getNestHost().getSimpleName() + " "
                + GenericsInner.class.isNestmateOf(Inner.Deeper.class));
        System.out.println("names " + Inner.Deeper.class.getName() + " " + Inner.Deeper.class.getSimpleName() + " "
                + Inner.class.getEnclosingClass().getSimpleName() + " " + Inner.class.isMemberClass());
    }
}
