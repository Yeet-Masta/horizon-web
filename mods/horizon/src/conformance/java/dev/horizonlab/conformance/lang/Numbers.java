package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.List;

/** Boxing caches and the wrapper classes' arithmetic helpers, integer edge cases (overflow,
 * division and shifts), floating-point conversions and comparisons. */
public class Numbers {
    static Integer box(int v) {
        return v;
    }

    static int unbox(Integer v) {
        return v;
    }

    static String overload(int x) {
        return "int";
    }

    static String overload(long x) {
        return "long";
    }

    static String overload(Integer x) {
        return "Integer";
    }

    static String overload(Object x) {
        return "Object";
    }

    static String overload(int... x) {
        return "varargs";
    }

    static String widen(double x) {
        return "double";
    }

    public static void main(String[] args) {
        System.out.println("Integer cache " + (box(127) == box(127)) + " " + (box(128) == box(128)) + " "
                + (box(-128) == box(-128)) + " " + (box(-129) == box(-129)) + " equals " + box(1000).equals(box(1000)));
        Long l1 = 127L;
        Long l2 = 127L;
        Long l3 = 128L;
        Long l4 = 128L;
        Short s1 = 100;
        Short s2 = 100;
        Character c1 = 'z';
        Character c2 = 'z';
        Character c3 = 'Ā';
        Character c4 = 'Ā';
        Byte b1 = -128;
        Byte b2 = -128;
        System.out.println("other caches " + (l1 == l2) + " " + (l3 == l4) + " " + (s1 == s2) + " " + (c1 == c2) + " "
                + (c3 == c4) + " " + (b1 == b2) + " " + (Boolean.valueOf(true) == Boolean.TRUE) + " "
                + (Integer.valueOf(5) == Integer.valueOf("5")));
        try {
            unbox(null);
        } catch (NullPointerException e) {
            System.out.println("unboxing null threw");
        }
        Integer nullable = null;
        try {
            int bad = true ? nullable : 0;
            System.out.println(bad);
        } catch (NullPointerException e) {
            System.out.println("ternary unboxing threw");
        }
        List<Integer> list = new ArrayList<>(List.of(1, 2, 3, 300));
        list.remove(Integer.valueOf(2));
        list.remove(0);
        System.out.println("remove(Object) vs remove(int) " + list);
        short sh = 5;
        byte by = 5;
        char ch = 'c';
        long lg = 5;
        System.out.println("overloads " + overload(sh) + " " + overload(by) + " " + overload(ch) + " " + overload(lg)
                + " " + overload(Integer.valueOf(1)) + " " + overload("s") + " " + overload() + " " + overload(1, 2)
                + " " + widen(1) + " " + widen(1L) + " " + widen('a'));

        System.out.println("parse " + Integer.parseInt("-123") + " " + Integer.parseInt("ff", 16) + " "
                + Integer.parseInt("+7") + " " + Long.parseLong("9223372036854775807") + " " + Integer.valueOf("0042")
                + " " + Double.parseDouble("1e3") + " " + Float.parseFloat("3.4028235E38") + " "
                + Double.parseDouble("-Infinity") + " " + Double.valueOf(".5") + " " + Boolean.parseBoolean("TRUE")
                + " " + Short.parseShort("-32768") + " " + Byte.parseByte("7f", 16) + " " + Integer.decode("0x1F")
                + " " + Long.decode("-010"));
        for (String bad : new String[] {"", "12a", "2147483648", " 1", "0x10"}) {
            try {
                Integer.parseInt(bad);
            } catch (NumberFormatException e) {
                System.out.println("NumberFormatException " + e.getMessage());
            }
        }
        System.out.println("toString " + Integer.toString(255, 16) + " " + Integer.toString(-255, 2) + " "
                + Integer.toHexString(-1) + " " + Integer.toBinaryString(10) + " " + Integer.toOctalString(8) + " "
                + Long.toHexString(-2L) + " " + Integer.toUnsignedString(-1) + " " + Long.toUnsignedString(-1L) + " "
                + Integer.parseUnsignedInt("4294967295") + " " + Integer.toString(Integer.MIN_VALUE, 36) + " "
                + Long.toString(Long.MAX_VALUE, 7));
        int x = 0b1011_0000;
        System.out.println("bits " + Integer.bitCount(x) + " " + Integer.reverse(x) + " " + Integer.reverseBytes(x) + " "
                + Integer.highestOneBit(x) + " " + Integer.lowestOneBit(x) + " " + Integer.numberOfLeadingZeros(x) + " "
                + Integer.numberOfTrailingZeros(x) + " " + Integer.rotateLeft(x, 28) + " " + Integer.signum(-x) + " "
                + Long.bitCount(-1L) + " " + Long.numberOfTrailingZeros(0) + " " + Long.reverse(1L) + " "
                + Integer.compare(3, 7) + " " + Long.compare(7, 3) + " " + Integer.compareUnsigned(-1, 1) + " "
                + Integer.divideUnsigned(-1, 3) + " " + Integer.remainderUnsigned(-1, 3) + " " + Integer.sum(2, 3)
                + " " + Integer.max(4, -4) + " " + Character.reverseBytes('a') + " " + Short.reverseBytes((short) 1));

        System.out.println("overflow " + (Integer.MAX_VALUE + 1) + " " + (Integer.MIN_VALUE / -1) + " "
                + (Integer.MIN_VALUE % -1) + " " + (Long.MIN_VALUE / -1) + " " + Math.abs(Integer.MIN_VALUE) + " "
                + (-Integer.MIN_VALUE) + " " + (Integer.MAX_VALUE * 2) + " " + (long) Integer.MAX_VALUE * 2);
        System.out.println("division " + (7 / 2) + " " + (-7 / 2) + " " + (7 % -3) + " " + (-7 % 3) + " "
                + Math.floorDiv(-7, 2) + " " + Math.floorMod(-7, 3) + " " + (7.5 % 2) + " " + (-7.5 % 2) + " "
                + Math.floorMod(7L, -3));
        int one = 1;
        long oneL = 1;
        System.out.println("shifts " + (one << 33) + " " + (oneL << 33) + " " + (-16 >> 2) + " " + (-16 >>> 28) + " "
                + (-16L >>> 60) + " " + (one << -1) + " " + (1 << 31) + " " + ((byte) 0x80 >> 1) + " "
                + ((byte) 0x80 >>> 1));
        byte bb = 10;
        bb += 300;
        short ss = (short) 70000;
        char cc = (char) -1;
        int fromChar = cc;
        System.out.println("narrowing " + bb + " " + ss + " " + fromChar + " " + (byte) 200 + " " + (short) -40000
                + " " + (char) 66 + " " + (int) 'A' + " " + (byte) 'é');
        double[] doubles = {1e20, -1e20, Double.NaN, 3.99, -3.99, Double.POSITIVE_INFINITY, 2147483647.5, -0.0};
        for (double dv : doubles) {
            System.out.println("d2i " + dv + " -> " + (int) dv + " " + (long) dv + " " + (float) dv + " " + (char) dv + " "
                    + (short) dv + " " + (byte) dv);
        }
        System.out.println("float " + (0.1f + 0.2f) + " " + (float) 0.1 + " " + (double) 0.1f + " " + (1.0f / 3) + " "
                + (int) 1e9f + " " + (1.0f * Integer.MAX_VALUE) + " " + (16777216f + 1f) + " " + (long) 1e19f);
        System.out.println("compare " + Double.compare(0.0, -0.0) + " " + (0.0 == -0.0) + " " + Double.valueOf(0.0).equals(-0.0)
                + " " + Double.compare(Double.NaN, 1) + " " + (Double.NaN == Double.NaN) + " "
                + Double.valueOf(Double.NaN).equals(Double.NaN) + " " + Math.min(0.0, -0.0) + " " + Math.max(Double.NaN, 1)
                + " " + Float.compare(1f, 2f) + " " + Double.isNaN(0.0 / 0) + " " + Double.isFinite(1 / 0.0) + " "
                + (Double.NaN > 1) + " " + (Double.NaN < 1) + " " + !(Double.NaN >= 1));
        System.out.println("bits " + Double.doubleToLongBits(1.0) + " " + Long.toHexString(Double.doubleToRawLongBits(-0.0))
                + " " + Float.floatToIntBits(1.5f) + " " + Float.intBitsToFloat(0x7f800000) + " "
                + Double.longBitsToDouble(0x3ff8000000000000L) + " " + Integer.toHexString(Float.floatToIntBits(Float.NaN))
                + " " + Double.toHexString(1.0) + " " + Math.ulp(1.0) + " " + Math.ulp(1f) + " " + Double.MIN_NORMAL);
        System.out.println("hashCodes " + Integer.hashCode(-5) + " " + Long.hashCode(1L << 40) + " " + Double.hashCode(1.5)
                + " " + Float.hashCode(1.5f) + " " + Boolean.hashCode(true) + " " + Boolean.hashCode(false) + " "
                + Character.hashCode('a') + " " + Short.hashCode((short) -1) + " " + Byte.hashCode((byte) -1) + " "
                + Double.valueOf(-0.0).hashCode());
        System.out.println("wrappers " + Integer.MAX_VALUE + " " + Long.MIN_VALUE + " " + Short.MAX_VALUE + " " + Byte.MIN_VALUE
                + " " + Float.MIN_VALUE + " " + Double.MAX_VALUE + " " + Integer.SIZE + " " + Long.BYTES + " "
                + Integer.valueOf(65).byteValue() + " " + Double.valueOf(3.9).intValue() + " " + Long.valueOf(1L << 33).intValue()
                + " " + Integer.valueOf(3).doubleValue() + " " + Integer.valueOf(7).compareTo(9) + " " + Character.MIN_RADIX);
        long acc = 0;
        for (int k = 0; k < 100; k++) {
            acc = acc * 31 + k;
            acc ^= acc >>> 7;
        }
        System.out.println("mix " + acc + " " + Long.rotateRight(acc, 13) + " " + Long.highestOneBit(acc) + " "
                + Math.multiplyHigh(acc, 0x9E3779B97F4A7C15L) + " " + Long.remainderUnsigned(acc, 1000) + " "
                + Long.divideUnsigned(acc, 1000));
    }
}
