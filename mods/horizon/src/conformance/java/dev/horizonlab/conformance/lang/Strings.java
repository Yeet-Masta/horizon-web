package dev.horizonlab.conformance.lang;

import java.util.Arrays;
import java.util.Locale;
import java.util.StringJoiner;

/** Strings: concatenation of every type, String.format's conversions and flags, the String and
 * StringBuilder methods mods use, interning, hash codes and surrogate pairs. */
public class Strings {
    static final String CONSTANT = "con" + "stant";

    @Override
    public String toString() {
        return "Strings!";
    }

    public static void main(String[] args) {
        char c = 'x';
        int i = -42;
        long l = 1L << 50;
        float f = 1.1f;
        double d = 2.0 / 3;
        boolean z = true;
        Object nothing = null;
        String concat = "c=" + c + " i=" + i + " l=" + l + " f=" + f + " d=" + d + " z=" + z + " null=" + nothing
                + " obj=" + new Strings() + " chars=" + 'a' + 'b' + " sum=" + ('a' + 'b') + " arr=" + new int[0].length;
        System.out.println(concat);
        System.out.println(1 + 2 + "3" + 4 + 5);
        byte b = -1;
        short s = 1000;
        System.out.println("byte " + b + " short " + s + " char+int " + (char) (c + 1) + " float max " + Float.MAX_VALUE
                + " double min " + Double.MIN_VALUE + " tiny " + 1e-5 + " big " + 1.0e7 + " " + 123456789.0f + " " + 100.0
                + " " + -0.0f + " " + (float) 1e10 + " " + 0.1f + " " + (0.1 + 0.2));

        System.out.println(String.format("|%d|%5d|%-5d|%05d|%+d|%,d|%x|%X|%o|%08X|", 42, 42, 42, 42, 42, 1234567, 255, 255,
                8, -1));
        System.out.println(String.format("|%s|%10s|%-10s|%.2s|%S|%b|%c|%%|%n", "str", "right", "left", "truncate", "up",
                null, 'q').replace("\n", "\\n"));
        System.out.println(String.format("|%f|%.2f|%10.3f|%-10.1f|%e|%.3E|%g|%a|", Math.PI, 2.675, -1.5, 0.05, 12345.678,
                0.000123, 0.0001, 1.0));
        System.out.println(String.format("|%.0f|%.0f|%.0f|%.1f|%.1f|%,.2f|%(.2f|", 0.5, 1.5, 2.5, 0.25, 0.35, 9876543.215,
                -3.14159));
        System.out.println(String.format("%2$s %1$s %2$s", "a", "b") + " " + String.format("%h", "hello") + " "
                + String.format("%d%%", 50) + " " + String.format("%5.1f%%", 99.44));
        System.out.println(String.format(Locale.ROOT, "%,d %.3f", 1000000, 1.0005) + " " + "x".formatted()
                + " " + "%s-%s".formatted(1, 2));
        System.out.printf("printf %s %d%n", "works", 7);

        String text = "  The quick brown fox jumps over the lazy dog  ";
        String trimmed = text.trim();
        System.out.println("[" + trimmed + "] " + trimmed.length() + " " + text.strip().equals(trimmed) + " ["
                + text.stripLeading() + "] [" + text.stripTrailing() + "]");
        System.out.println(trimmed.substring(4, 9) + " " + trimmed.indexOf('o') + " " + trimmed.indexOf("o", 13) + " "
                + trimmed.lastIndexOf("the") + " " + trimmed.indexOf("cat") + " " + trimmed.charAt(10));
        System.out.println(trimmed.toUpperCase(Locale.ROOT) + " " + trimmed.toLowerCase(Locale.ROOT).replace('o', '0'));
        System.out.println(trimmed.replace("the", "a") + " | " + trimmed.replaceAll("[aeiou]", "*") + " | "
                + trimmed.replaceFirst("\\s+", "_"));
        System.out.println(Arrays.toString(trimmed.split(" ")) + " " + Arrays.toString("a,b,,c,,".split(","))
                + " " + Arrays.toString("a,b,,c,,".split(",", -1)) + " " + Arrays.toString("a1b22c333".split("\\d+"))
                + " " + Arrays.toString("x".split(",")) + " " + Arrays.toString("".split(",")) + " "
                + Arrays.toString("a:b:c".split(":", 2)));
        System.out.println(String.join("-", "a", "b", "c") + " " + String.join("/", Arrays.asList("x", "y")) + " "
                + "ab".repeat(3) + " [" + "".repeat(5) + "] " + "abc".contains("bc") + " " + "abc".startsWith("ab")
                + " " + "abc".endsWith("bc") + " " + "abc".startsWith("c", 2) + " " + "Hello".equalsIgnoreCase("hELLO")
                + " " + "  ".isBlank() + " " + "".isEmpty());
        System.out.println("apple".compareTo("banana") + " " + "b".compareTo("a") + " " + "abc".compareTo("abcd") + " "
                + "ABC".compareToIgnoreCase("abd") + " " + "hello".hashCode() + " " + "".hashCode() + " "
                + "The quick brown fox".hashCode() + " " + "été".hashCode());
        System.out.println(String.valueOf(new char[] {'h', 'i'}) + " " + String.valueOf(3.0) + " " + String.valueOf(true)
                + " " + String.valueOf((Object) null) + " " + new String(new char[] {'a', 'b', 'c'}, 1, 2) + " "
                + Arrays.toString("hey".toCharArray()) + " " + "hey".chars().sum() + " " + "a-b".matches("[a-z]-[a-z]"));
        System.out.println("lines " + "one\ntwo\r\nthree".lines().count() + " indent [" + "x\ny".indent(2).replace("\n", "|")
                + "] translate " + "a\\tb".translateEscapes().length() + " " + "Hello".chars().filter(Character::isUpperCase).count());

        String literal = "constant";
        String built = new StringBuilder("con").append("stant").toString();
        System.out.println("intern " + (literal == CONSTANT) + " " + (literal == built) + " " + (literal == built.intern())
                + " " + literal.equals(built));

        StringBuilder sb = new StringBuilder();
        sb.append(1).append('a').append(2.5).append(true).append((Object) null).append(new char[] {'x', 'y'}).append(3L)
                .append(1.5f);
        System.out.println(sb + " " + sb.length());
        sb.insert(0, "start:").insert(6, 99).reverse();
        System.out.println(sb);
        sb.setLength(5);
        sb.setCharAt(0, 'Z');
        sb.deleteCharAt(1).delete(1, 2).replace(0, 1, "QQ");
        System.out.println(sb + " " + sb.indexOf("Q") + " " + sb.charAt(1) + " " + new StringBuilder("racecar").reverse());
        StringBuilder cap = new StringBuilder(4);
        for (int k = 0; k < 20; k++) {
            cap.append(k % 10);
        }
        System.out.println(cap + " " + cap.substring(5, 8) + " " + cap.lastIndexOf("9") + " " + cap.compareTo(new StringBuilder("0")));
        StringJoiner joiner = new StringJoiner(", ", "{", "}");
        joiner.setEmptyValue("EMPTY");
        System.out.println(joiner);
        joiner.add("one").add("two");
        System.out.println(joiner + " " + joiner.length());

        String emoji = "a😀b";
        System.out.println(emoji.length() + " " + emoji.codePointCount(0, emoji.length()) + " " + emoji.codePointAt(1) + " "
                + Character.charCount(emoji.codePointAt(1)) + " " + new StringBuilder(emoji).reverse().codePointAt(1) + " "
                + Character.isSurrogate(emoji.charAt(1)) + " " + Character.toChars(0x1F600).length + " "
                + new String(Character.toChars(65)) + " " + emoji.codePoints().count());
        String accent = "École ß";
        System.out.println(accent.toUpperCase(Locale.ROOT) + " " + accent.toLowerCase(Locale.ROOT) + " " + accent.length());
        String block = """
                Text block
                  indented \
                joined
                end""";
        System.out.println(block);
        System.out.println(Character.isDigit('7') + " " + Character.isLetter('x') + " " + Character.isLetterOrDigit('_')
                + " " + Character.isWhitespace('\t') + " " + Character.toUpperCase('q') + " " + Character.getNumericValue('z')
                + " " + Character.digit('f', 16) + " " + Character.forDigit(11, 16) + " " + (int) Character.MAX_VALUE + " "
                + Character.isUpperCase('A') + " " + Character.toString(97) + " " + Character.valueOf('c').compareTo('a'));
    }
}
