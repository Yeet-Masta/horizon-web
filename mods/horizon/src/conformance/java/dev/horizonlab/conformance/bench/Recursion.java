package dev.horizonlab.conformance.bench;

/** Calls and integer arithmetic: naive Fibonacci and Ackermann. */
public class Recursion {
    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static int ackermann(int m, int n) {
        if (m == 0) {
            return n + 1;
        }
        if (n == 0) {
            return ackermann(m - 1, 1);
        }
        return ackermann(m - 1, ackermann(m, n - 1));
    }

    public static void main(String[] args) {
        System.out.println(fib(27) + " " + ackermann(2, 2000));
    }
}
