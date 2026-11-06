package dev.horizonlab.conformance.lang;

/** System.exit ends the program at once, with its status, and skips the rest of main (a finally
 * included). */
public class Exit {
    public static void main(String[] args) {
        System.out.println("exiting");
        try {
            System.exit(3);
        } finally {
            System.out.println("finally after exit");
        }
    }
}
