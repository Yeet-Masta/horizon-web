package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.List;

/** Exceptions: finally ordering and overriding, nesting, multi-catch, causes, suppressed
 * exceptions from try-with-resources, the exceptions the VM itself throws with their messages, a
 * caught StackOverflowError, and stack traces built from the LineNumberTable. */
public class Exceptions {
    static final List<String> log = new ArrayList<>();

    static class AppException extends Exception {
        final int code;

        AppException(String message, int code) {
            super(message);
            this.code = code;
        }

        AppException(String message, Throwable cause) {
            super(message, cause);
            this.code = -1;
        }
    }

    static class Resource implements AutoCloseable {
        private final String name;
        private final boolean failOnClose;

        Resource(String name, boolean failOnClose) {
            this.name = name;
            this.failOnClose = failOnClose;
            log.add("open " + name);
        }

        @Override
        public void close() throws AppException {
            log.add("close " + name);
            if (failOnClose) {
                throw new AppException("close failed: " + name, 2);
            }
        }
    }

    @SuppressWarnings("finally")
    static int finallyWins() {
        try {
            return 1;
        } finally {
            return 2;
        }
    }

    static int finallyRuns() {
        int x = 10;
        try {
            return x;
        } finally {
            x = 20;
            log.add("finally saw " + x);
        }
    }

    static String nested(int mode) {
        StringBuilder sb = new StringBuilder();
        try {
            sb.append("a");
            try {
                sb.append("b");
                if (mode == 1) {
                    throw new IllegalStateException("inner");
                }
                if (mode == 2) {
                    throw new UnsupportedOperationException("outer");
                }
                sb.append("c");
            } catch (IllegalStateException e) {
                sb.append("[caught ").append(e.getMessage()).append("]");
            } finally {
                sb.append("d");
            }
            sb.append("e");
        } catch (RuntimeException e) {
            sb.append("[outer caught ").append(e.getMessage()).append("]");
        } finally {
            sb.append("f");
        }
        return sb.toString();
    }

    static int depth;

    static void recurse() {
        depth++;
        recurse();
    }

    static void thrower() throws AppException {
        throw new AppException("from thrower", 7);
    }

    static void wrapper() throws AppException {
        try {
            thrower();
        } catch (AppException e) {
            throw new AppException("wrapped", e);
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("finallyWins " + finallyWins());
        System.out.println("finallyRuns " + finallyRuns() + " " + log);
        log.clear();
        for (int mode = 0; mode < 3; mode++) {
            System.out.println("nested " + mode + " " + nested(mode));
        }

        try (Resource a = new Resource("a", false); Resource b = new Resource("b", true)) {
            log.add("body");
            throw new IllegalStateException("body failed");
        } catch (IllegalStateException e) {
            System.out.println("twr caught " + e.getMessage() + " suppressed " + e.getSuppressed().length + " "
                    + e.getSuppressed()[0].getMessage());
        }
        System.out.println(log);
        log.clear();
        try (Resource c = new Resource("c", true)) {
            log.add("body ok");
        } catch (AppException e) {
            System.out.println("close exception " + e.getMessage() + " code " + e.code);
        }
        System.out.println(log);

        try {
            wrapper();
        } catch (AppException e) {
            Throwable cause = e.getCause();
            System.out.println(e.getMessage() + " <- " + cause.getMessage() + " " + (cause instanceof AppException)
                    + " " + e);
            StackTraceElement top = cause.getStackTrace()[0];
            System.out.println("top frame " + top.getClassName() + "." + top.getMethodName() + " line "
                    + top.getLineNumber() + " " + top.getFileName());
            StackTraceElement[] trace = e.getStackTrace();
            System.out.println("wrapper frame " + trace[0].getMethodName() + ":" + trace[0].getLineNumber()
                    + " caller " + trace[1].getMethodName() + ":" + trace[1].getLineNumber());
        }

        Object[] attempts = new Object[8];
        for (int i = 0; i < attempts.length; i++) {
            try {
                switch (i) {
                    case 0 -> {
                        int[] arr = new int[3];
                        arr[5] = 1;
                    }
                    case 1 -> {
                        int zero = 0;
                        System.out.println(10 / zero);
                    }
                    case 2 -> {
                        Object o = "string";
                        Integer n = (Integer) o;
                        System.out.println(n);
                    }
                    case 3 -> {
                        int[] neg = new int[-1];
                        System.out.println(neg.length);
                    }
                    case 4 -> {
                        Object[] objs = new String[1];
                        objs[0] = 1;
                    }
                    case 5 -> {
                        String s = null;
                        System.out.println(s.length());
                    }
                    case 6 -> {
                        long zero = 0;
                        System.out.println(5L % zero);
                    }
                    case 7 -> {
                        List<Integer> immutable = List.of(1);
                        immutable.add(2);
                    }
                    default -> {}
                }
            } catch (ClassCastException e) {
                System.out.println(i + " ClassCastException");
            } catch (ArithmeticException | ArrayIndexOutOfBoundsException | NegativeArraySizeException
                    | ArrayStoreException | NullPointerException e) {
                System.out.println(i + " " + e.getClass().getName() + ": " + e.getMessage());
            } catch (UnsupportedOperationException e) {
                System.out.println(i + " UnsupportedOperationException " + e.getMessage());
            }
        }
        System.out.println("double division " + (1.0 / 0) + " " + (0.0 / 0) + " " + (-1 % 1.0));

        try {
            recurse();
        } catch (StackOverflowError e) {
            System.out.println("StackOverflowError caught, depth positive " + (depth > 100));
        }
        try {
            throw new Error("custom error");
        } catch (Throwable t) {
            System.out.println("throwable " + t);
        }
        Exception chained = new RuntimeException(new IllegalArgumentException("root"));
        System.out.println("chained " + chained.getMessage() + " | " + chained);
        Exception noMessage = new IllegalStateException();
        System.out.println("no message " + noMessage.getMessage() + " " + noMessage + " "
                + noMessage.getLocalizedMessage());
        Throwable initCause = new Exception("late").initCause(new Error("cause"));
        System.out.println("initCause " + initCause.getCause());
        try {
            initCause.initCause(new Error("again"));
        } catch (IllegalStateException e) {
            System.out.println("initCause twice: " + e.getMessage().startsWith("Can't overwrite cause"));
        }
        try {
            Object o = null;
            synchronized (o) {
                System.out.println("unreachable");
            }
        } catch (NullPointerException e) {
            System.out.println("synchronized null " + e.getClass().getSimpleName());
        }
        int result = 0;
        outer:
        for (int i = 0; i < 5; i++) {
            try {
                for (int j = 0; j < 5; j++) {
                    if (i * j == 6) {
                        break outer;
                    }
                    if (j == i) {
                        continue outer;
                    }
                    result += j;
                }
            } finally {
                result += 100;
            }
        }
        System.out.println("labeled with finally " + result);
    }
}
