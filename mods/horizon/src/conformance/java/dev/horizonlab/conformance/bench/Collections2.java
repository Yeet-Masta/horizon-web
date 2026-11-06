package dev.horizonlab.conformance.bench;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/** Boxed collections: HashMap and TreeMap inserts and lookups, an ArrayList sort. */
public class Collections2 {
    public static void main(String[] args) {
        Map<Integer, Integer> hash = new HashMap<>();
        for (int i = 0; i < 100_000; i++) {
            hash.merge(i * 7919 % 50_000, i, Integer::sum);
        }
        long sum = 0;
        for (int i = 0; i < 100_000; i++) {
            Integer v = hash.get(i);
            if (v != null) {
                sum += v;
            }
        }
        TreeMap<String, Integer> tree = new TreeMap<>();
        for (int i = 0; i < 20_000; i++) {
            tree.put("key" + (i * 31 % 20_000), i);
        }
        List<Integer> list = new ArrayList<>();
        for (int i = 0; i < 100_000; i++) {
            list.add((i * 48271) % 100_003);
        }
        Collections.sort(list);
        System.out.println(hash.size() + " " + sum + " " + tree.firstKey() + " " + tree.lastEntry().getValue() + " "
                + list.get(50_000));
    }
}
