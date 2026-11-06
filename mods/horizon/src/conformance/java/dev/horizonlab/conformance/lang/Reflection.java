package dev.horizonlab.conformance.lang;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Proxy;
import java.lang.reflect.RecordComponent;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.Map;

/** Reflection as config libraries and loaders use it: class mirrors, declared members, get/set/
 * invoke on private members, constructors, annotations and their defaults, enums, records,
 * generic signatures, Class.forName and dynamic proxies. Member lists are sorted: their order is
 * not specified. */
public class Reflection {
    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.TYPE, ElementType.FIELD, ElementType.METHOD})
    @interface Config {
        String value();

        int priority() default 5;

        String[] tags() default {};

        Level level() default Level.LOW;
    }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Range {
        double min();

        double max();
    }

    @Retention(RetentionPolicy.CLASS)
    @interface Invisible {}

    enum Level { LOW, HIGH }

    interface Greeter {
        String greet(String name);

        int twice(int x);
    }

    static class Base {
        protected int baseField = 1;

        public String hello() {
            return "base hello";
        }
    }

    @Config(value = "settings", tags = {"a", "b"}, level = Level.HIGH)
    public static class Settings extends Base implements Comparable<Settings> {
        @Config("speed")
        @Range(min = 0, max = 10)
        private double speed = 2.5;
        @Invisible
        public static int instances;
        protected final List<String> names = new ArrayList<>();
        transient volatile long counter;
        private final Map<String, List<Integer>> table = Map.of();

        public Settings() {
            instances++;
        }

        private Settings(double speed) {
            this();
            this.speed = speed;
        }

        @Config(value = "boost", priority = 9)
        public double boost(double factor) {
            return speed * factor;
        }

        private static String secret(String a, int b) {
            return a + b;
        }

        public void fail() {
            throw new IllegalStateException("failed inside");
        }

        @Override
        public int compareTo(Settings o) {
            return Double.compare(speed, o.speed);
        }
    }

    record Pair(String left, int right) {}

    static String names(Object[] members) {
        List<String> out = new ArrayList<>();
        for (Object member : members) {
            if (member instanceof Field f) {
                out.add(Modifier.toString(f.getModifiers()) + " " + f.getType().getSimpleName() + " " + f.getName());
            } else if (member instanceof Method m) {
                out.add(Modifier.toString(m.getModifiers()) + " " + m.getReturnType().getSimpleName() + " " + m.getName()
                        + Arrays.toString(Arrays.stream(m.getParameterTypes()).map(Class::getSimpleName).toArray()));
            } else if (member instanceof Constructor<?> c) {
                out.add(Modifier.toString(c.getModifiers()) + " <init>" + Arrays.toString(
                        Arrays.stream(c.getParameterTypes()).map(Class::getSimpleName).toArray()));
            }
        }
        out.sort(Comparator.naturalOrder());
        return out.toString();
    }

    public static void main(String[] args) throws Exception {
        Class<?> cls = Settings.class;
        System.out.println(cls.getName() + " | " + cls.getSimpleName() + " | " + cls.getSuperclass().getSimpleName() + " | "
                + Arrays.toString(cls.getInterfaces()) + " | " + Modifier.toString(cls.getModifiers()) + " | "
                + cls.getPackageName() + " | " + cls.isInterface() + " " + cls.isEnum() + " " + cls.isRecord() + " "
                + cls.isArray() + " " + cls.isPrimitive());
        System.out.println("fields " + names(cls.getDeclaredFields()));
        System.out.println("public fields " + names(cls.getFields()));
        System.out.println("methods " + names(cls.getDeclaredMethods()));
        System.out.println("constructors " + names(cls.getDeclaredConstructors()));

        Settings settings = (Settings) cls.getConstructor().newInstance();
        Field speed = cls.getDeclaredField("speed");
        speed.setAccessible(true);
        System.out.println("get " + speed.get(settings) + " " + speed.getDouble(settings));
        speed.set(settings, 7.5);
        speed.setDouble(settings, speed.getDouble(settings) + 1);
        Method boost = cls.getMethod("boost", double.class);
        System.out.println("invoke " + boost.invoke(settings, 2.0) + " " + boost.getReturnType() + " "
                + boost.getParameterCount());
        Method secret = cls.getDeclaredMethod("secret", String.class, int.class);
        secret.setAccessible(true);
        System.out.println("static invoke " + secret.invoke(null, "x", 41));
        Constructor<?> privateCtor = cls.getDeclaredConstructor(double.class);
        privateCtor.setAccessible(true);
        Settings other = (Settings) privateCtor.newInstance(3.0);
        System.out.println("private ctor " + other.boost(1) + " instances " + Settings.instances + " static field "
                + cls.getField("instances").getInt(null));
        try {
            cls.getMethod("fail").invoke(settings);
        } catch (InvocationTargetException e) {
            System.out.println("InvocationTargetException cause " + e.getCause());
        }
        try {
            cls.getMethod("nope");
        } catch (NoSuchMethodException e) {
            System.out.println("NoSuchMethodException " + e.getMessage());
        }
        try {
            boost.invoke(settings, "wrong");
        } catch (IllegalArgumentException e) {
            System.out.println("IllegalArgumentException on bad argument");
        }
        System.out.println("inherited " + cls.getMethod("hello").invoke(settings) + " declaring "
                + cls.getMethod("hello").getDeclaringClass().getSimpleName() + " field " + Base.class.getDeclaredField("baseField")
                        .getInt(settings));

        Config config = cls.getAnnotation(Config.class);
        System.out.println("class annotation " + config.value() + " " + config.priority() + " " + Arrays.toString(config.tags())
                + " " + config.level() + " present " + cls.isAnnotationPresent(Config.class) + " count "
                + cls.getDeclaredAnnotations().length);
        System.out.println("toString " + config);
        Config speedConfig = speed.getAnnotation(Config.class);
        Range range = speed.getAnnotation(Range.class);
        System.out.println("field annotations " + speedConfig.value() + " " + speedConfig.priority() + " " + range.min() + ".."
                + range.max() + " invisible " + cls.getField("instances").getAnnotation(Invisible.class) + " "
                + speed.getAnnotations().length);
        System.out.println("method annotation " + boost.getAnnotation(Config.class).priority() + " equals "
                + config.equals(cls.getAnnotation(Config.class)) + " type " + config.annotationType().getSimpleName());

        Field names = cls.getDeclaredField("names");
        Field table = cls.getDeclaredField("table");
        System.out.println("generic " + names.getGenericType() + " | " + table.getGenericType() + " | "
                + ((ParameterizedType) table.getGenericType()).getActualTypeArguments()[1] + " | "
                + cls.getGenericInterfaces()[0]);

        System.out.println("enum " + Arrays.toString(Level.class.getEnumConstants()) + " " + Enum.valueOf(Level.class, "HIGH")
                + " " + Level.class.isEnum() + " " + Level.HIGH.getClass().getSuperclass().getSimpleName());
        RecordComponent[] components = Pair.class.getRecordComponents();
        Pair pair = new Pair("l", 9);
        for (RecordComponent component : components) {
            System.out.println("component " + component.getName() + " " + component.getType().getSimpleName() + " "
                    + component.getAccessor().invoke(pair));
        }
        Pair made = (Pair) Pair.class.getDeclaredConstructor(String.class, int.class).newInstance("m", 3);
        System.out.println("record ctor " + made);

        Class<?> byName = Class.forName("dev.horizonlab.conformance.lang.Reflection$Settings");
        System.out.println("forName " + (byName == cls) + " " + Class.forName("java.util.ArrayList").getSimpleName() + " "
                + int[].class.getName() + " " + String[][].class.getName() + " " + int[].class.getComponentType() + " "
                + int.class.getName() + " " + Integer.TYPE + " " + void.class + " " + Class.forName("[Ljava.lang.String;")
                        .getSimpleName());
        try {
            Class.forName("dev.horizonlab.conformance.lang.Missing");
        } catch (ClassNotFoundException e) {
            System.out.println("ClassNotFoundException " + e.getMessage());
        }
        System.out.println("isInstance " + Base.class.isInstance(settings) + " " + Base.class.isAssignableFrom(cls) + " "
                + cls.isAssignableFrom(Base.class) + " " + Comparable.class.isAssignableFrom(cls) + " "
                + Object.class.isAssignableFrom(int[].class) + " cast " + Base.class.cast(settings).baseField + " "
                + Number.class.isInstance(5));
        try {
            String.class.cast(5);
        } catch (ClassCastException e) {
            System.out.println("Class.cast threw");
        }
        Object array = java.lang.reflect.Array.newInstance(int.class, 3);
        java.lang.reflect.Array.setInt(array, 1, 42);
        System.out.println("Array " + java.lang.reflect.Array.getLength(array) + " " + java.lang.reflect.Array.get(array, 1));

        List<String> calls = new ArrayList<>();
        Greeter proxy = (Greeter) Proxy.newProxyInstance(Reflection.class.getClassLoader(), new Class<?>[] {Greeter.class},
                (p, method, margs) -> {
                    calls.add(method.getName());
                    return switch (method.getName()) {
                        case "greet" -> "proxied " + margs[0];
                        case "twice" -> (Integer) margs[0] * 2;
                        case "toString" -> "GreeterProxy";
                        default -> null;
                    };
                });
        System.out.println(proxy.greet("you") + " " + proxy.twice(21) + " " + proxy + " " + calls + " "
                + Proxy.isProxyClass(proxy.getClass()));
    }
}
