package dev.horizonlab.conformance.bench;

/** Array loops: a sieve, a matrix product and an insertion sort over a fixed LCG sequence. */
public class Arrays1 {
    public static void main(String[] args) {
        boolean[] composite = new boolean[1_000_001];
        int primes = 0;
        for (int i = 2; i < composite.length; i++) {
            if (!composite[i]) {
                primes++;
                for (long j = (long) i * i; j < composite.length; j += i) {
                    composite[(int) j] = true;
                }
            }
        }
        int n = 60;
        double[][] a = new double[n][n];
        double[][] b = new double[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = (i * 31 + j) % 17 / 7.0;
                b[i][j] = (j * 13 + i) % 11 / 3.0;
            }
        }
        double trace = 0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                double sum = 0;
                for (int k = 0; k < n; k++) {
                    sum += a[i][k] * b[k][j];
                }
                if (i == j) {
                    trace += sum;
                }
            }
        }
        int[] values = new int[3000];
        long seed = 42;
        for (int i = 0; i < values.length; i++) {
            seed = (seed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
            values[i] = (int) (seed >>> 17);
        }
        for (int i = 1; i < values.length; i++) {
            int v = values[i];
            int j = i - 1;
            while (j >= 0 && values[j] > v) {
                values[j + 1] = values[j];
                j--;
            }
            values[j + 1] = v;
        }
        System.out.println(primes + " " + trace + " " + values[0] + " " + values[values.length - 1]);
    }
}
