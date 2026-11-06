package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.List;

/** Class and instance initialization order (JVMS 5.5): what triggers a class's <clinit> and what
 * does not, superclass before subclass, interfaces left alone, constants inlined, a failed
 * initializer, and instance initializers around constructor chaining. */
public class StaticInit {
    static final List<String> log = new ArrayList<>();

    static String note(String s) {
        log.add(s);
        return s;
    }

    static class Parent {
        static String p1 = note("Parent.p1");

        static {
            note("Parent.static");
        }

        String i1 = note("Parent.i1");

        {
            note("Parent.instance");
        }

        Parent() {
            note("Parent()");
            describe();
        }

        void describe() {
            note("Parent.describe");
        }
    }

    static class Child extends Parent {
        static String c1 = note("Child.c1");
        static final String CONSTANT = "inlined";
        static final String COMPUTED = note("Child.COMPUTED");
        String field = note("Child.field");

        static {
            note("Child.static");
        }

        Child() {
            this(1);
            note("Child()");
        }

        Child(int x) {
            super();
            note("Child(int)");
        }

        @Override
        void describe() {
            note("Child.describe field=" + field);
        }

        static void touch() {
            note("Child.touch");
        }
    }

    interface Iface {
        String VALUE = note("Iface.VALUE");

        default void hi() {}
    }

    interface Marker {
        String MARK = note("Marker.MARK");
    }

    static class Impl implements Iface, Marker {
        static {
            note("Impl.static");
        }
    }

    static class Lazy {
        static {
            note("Lazy.static");
        }

        static int value = 3;
    }

    static class Broken {
        static int value = compute();

        static int compute() {
            note("Broken.compute");
            throw new IllegalStateException("broken initializer");
        }
    }

    static class Cycle {
        static int a = CycleB.b + 1;
    }

    static class CycleB {
        static int b = Cycle.a + 10;
    }

    static void step(String what, Runnable action) {
        log.clear();
        action.run();
        System.out.println(what + ": " + log);
    }

    public static void main(String[] args) {
        step("constant", () -> note("read " + Child.CONSTANT));
        step("array", () -> note("array " + new Child[2].length));
        step("class literal", () -> note("literal " + Child.class.getSimpleName()));
        step("static method", Child::touch);
        step("again", Child::touch);
        step("new", () -> new Child());
        step("interface impl", () -> new Impl());
        step("interface field", () -> note("got " + Iface.VALUE));
        step("lazy field", () -> note("value " + Lazy.value));
        step("forName", () -> {
            try {
                Class.forName("dev.horizonlab.conformance.lang.StaticInit$Marker");
            } catch (ClassNotFoundException e) {
                throw new RuntimeException(e);
            }
        });
        step("broken", () -> {
            try {
                note("value " + Broken.value);
            } catch (ExceptionInInitializerError e) {
                note("ExceptionInInitializerError " + e.getCause().getMessage());
            }
            try {
                note("value " + Broken.value);
            } catch (NoClassDefFoundError e) {
                note("NoClassDefFoundError " + e.getMessage().contains("Broken"));
            }
        });
        step("cycle", () -> note("a=" + Cycle.a + " b=" + CycleB.b));
    }
}
