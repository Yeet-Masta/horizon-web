package dev.horizonlab.conformance.lang;

/** An exception that leaves main: stdout keeps what was printed, the launcher reports the
 * exception on stderr and exits 1. */
public class Uncaught {
    static void deeper(int n) {
        if (n == 0) {
            throw new IllegalStateException("gave up at the bottom");
        }
        deeper(n - 1);
    }

    public static void main(String[] args) {
        System.out.println("before");
        deeper(3);
        System.out.println("after");
    }
}
