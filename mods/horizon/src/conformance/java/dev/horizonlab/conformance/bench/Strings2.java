package dev.horizonlab.conformance.bench;

/** String building: StringBuilder appends, concatenation, String.format, split and hashCode. */
public class Strings2 {
    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 50_000; i++) {
            sb.append(i).append(',');
        }
        String joined = sb.toString();
        int hash = 0;
        for (String part : joined.split(",")) {
            hash = hash * 31 + part.hashCode();
        }
        String ids = "";
        int total = 0;
        for (int i = 0; i < 3000; i++) {
            ids = "mod:item_" + i + "/" + (i % 7);
            total += ids.length();
        }
        int formatted = 0;
        for (int i = 0; i < 3000; i++) {
            formatted += String.format("%s_%04d:%.2f", "block", i, i / 3.0).length();
        }
        System.out.println(joined.length() + " " + hash + " " + total + " " + formatted + " " + ids);
    }
}
