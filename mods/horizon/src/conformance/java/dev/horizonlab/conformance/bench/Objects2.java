package dev.horizonlab.conformance.bench;

/** Allocation and virtual dispatch: short-lived objects through an interface and a class hierarchy. */
public class Objects2 {
    interface Shape {
        double area();
    }

    record Circle(double r) implements Shape {
        public double area() {
            return 3.0 * r * r;
        }
    }

    record Square(double s) implements Shape {
        public double area() {
            return s * s;
        }
    }

    static final class Node {
        final int value;
        final Node next;

        Node(int value, Node next) {
            this.value = value;
            this.next = next;
        }
    }

    public static void main(String[] args) {
        double total = 0;
        for (int i = 0; i < 300_000; i++) {
            Shape shape = (i & 1) == 0 ? new Circle(i % 10) : new Square(i % 7);
            total += shape.area();
        }
        long sum = 0;
        for (int round = 0; round < 20; round++) {
            Node head = null;
            for (int i = 0; i < 10_000; i++) {
                head = new Node(i, head);
            }
            for (Node n = head; n != null; n = n.next) {
                sum += n.value;
            }
        }
        System.out.println(total + " " + sum);
    }
}
