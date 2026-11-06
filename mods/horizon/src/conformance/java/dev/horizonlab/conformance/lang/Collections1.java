package dev.horizonlab.conformance.lang;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.ConcurrentModificationException;
import java.util.Deque;
import java.util.HashMap;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;
import java.util.Map;
import java.util.NavigableMap;
import java.util.Objects;
import java.util.PriorityQueue;
import java.util.Random;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

/** Collections, and above all their iteration order, which mods' output and registration order
 * depend on: HashMap's hash spreading, table growth and treeified bins, LinkedHashMap's access
 * order, TreeMap's navigation, PriorityQueue's heap layout. */
public class Collections1 {
    /** A key whose hash collides on purpose, so one HashMap bin grows past the treeify threshold. */
    record Colliding(int id) implements Comparable<Colliding> {
        @Override
        public int hashCode() {
            return id % 3;
        }

        @Override
        public int compareTo(Colliding o) {
            return Integer.compare(id, o.id);
        }
    }

    public static void main(String[] args) {
        Map<String, Integer> hash = new HashMap<>();
        String[] words = "the quick brown fox jumps over the lazy dog while seven zebras quietly vex jaunty pigs".split(" ");
        for (int i = 0; i < words.length; i++) {
            hash.merge(words[i], i, Integer::sum);
        }
        System.out.println("HashMap " + hash);
        Map<Integer, String> ints = new HashMap<>();
        for (int i = 0; i < 40; i++) {
            ints.put(i * 37 - 500, "v" + i);
        }
        System.out.println("HashMap ints " + ints.keySet());
        Map<Long, Long> longs = new HashMap<>();
        for (long i = 1; i < 1L << 40; i *= 7) {
            longs.put(i, -i);
        }
        System.out.println("HashMap longs " + longs.keySet());
        Set<String> set = new HashSet<>();
        for (int i = 0; i < 30; i++) {
            set.add(Integer.toString(i * i, 7));
        }
        System.out.println("HashSet " + set);
        Map<Colliding, Integer> collide = new HashMap<>();
        for (int i = 20; i > 0; i--) {
            collide.put(new Colliding(i), i);
        }
        System.out.println("treeified " + collide.keySet());
        Set<Double> doubles = new HashSet<>(List.of(0.5, -0.0, 0.0, 1e10, Double.NaN, 3.25));
        System.out.println("HashSet doubles " + doubles);
        Map<Character, Boolean> chars = new HashMap<>();
        for (char c : "zyxwvutsrqponmlkjihgfedcba".toCharArray()) {
            chars.put(c, c % 2 == 0);
        }
        System.out.println("HashMap chars " + chars.keySet());
        HashMap<String, Integer> small = new HashMap<>(2, 0.5f);
        small.put("one", 1);
        small.put("two", 2);
        small.put("three", 3);
        small.remove("two");
        small.putIfAbsent("four", 4);
        small.computeIfAbsent("five", String::length);
        small.computeIfPresent("one", (k, v) -> v + 10);
        small.compute("six", (k, v) -> v == null ? 6 : v);
        small.replaceAll((k, v) -> v * 2);
        System.out.println(small + " " + small.getOrDefault("zero", -1) + " " + small.containsValue(12) + " "
                + small.containsKey("two"));

        LinkedHashMap<String, Integer> lru = new LinkedHashMap<>(16, 0.75f, true) {
            @Override
            protected boolean removeEldestEntry(Map.Entry<String, Integer> eldest) {
                return size() > 3;
            }
        };
        lru.put("a", 1);
        lru.put("b", 2);
        lru.put("c", 3);
        lru.get("a");
        lru.put("d", 4);
        System.out.println("LRU " + lru);
        LinkedHashSet<Integer> linkedSet = new LinkedHashSet<>(List.of(5, 3, 9, 1));
        linkedSet.add(3);
        linkedSet.add(0);
        System.out.println("LinkedHashSet " + linkedSet + " first " + linkedSet.getFirst() + " reversed " + linkedSet.reversed());

        TreeMap<String, Integer> tree = new TreeMap<>(hash);
        System.out.println("TreeMap " + tree.firstKey() + " " + tree.lastEntry() + " floor(m)=" + tree.floorKey("m")
                + " ceiling(m)=" + tree.ceilingKey("m") + " higher(the)=" + tree.higherKey("the") + " lower(a)="
                + tree.lowerKey("a"));
        System.out.println("headMap " + tree.headMap("fox") + " tail " + tree.tailMap("v", false).keySet());
        NavigableMap<String, Integer> descending = tree.descendingMap();
        System.out.println("descending " + descending.keySet() + " sub " + tree.subMap("d", true, "q", true));
        System.out.println("pollFirst " + tree.pollFirstEntry() + " " + tree.size());
        TreeMap<String, Integer> byLength = new TreeMap<>((a, b) -> a.length() != b.length() ? a.length() - b.length()
                : a.compareTo(b));
        byLength.putAll(hash);
        System.out.println("comparator TreeMap " + byLength.keySet());
        TreeSet<Integer> treeSet = new TreeSet<>(Collections.reverseOrder());
        treeSet.addAll(List.of(5, 1, 9, 3, 7));
        System.out.println("TreeSet " + treeSet + " " + treeSet.first() + " " + treeSet.headSet(5) + " "
                + treeSet.ceiling(4) + " " + treeSet.descendingSet());

        PriorityQueue<Integer> pq = new PriorityQueue<>();
        for (int v : new int[] {5, 3, 8, 1, 9, 2, 7}) {
            pq.add(v);
        }
        System.out.print("PriorityQueue " + pq + " polled");
        while (!pq.isEmpty()) {
            System.out.print(" " + pq.poll());
        }
        System.out.println();
        PriorityQueue<String> maxHeap = new PriorityQueue<>(Collections.reverseOrder());
        maxHeap.addAll(List.of("m", "z", "a", "q"));
        System.out.println("max heap " + maxHeap + " peek " + maxHeap.peek());

        Deque<Integer> deque = new ArrayDeque<>();
        deque.push(1);
        deque.addLast(2);
        deque.offerFirst(0);
        deque.add(3);
        System.out.println("ArrayDeque " + deque + " " + deque.peekLast() + " " + deque.pop() + " " + deque.pollLast() + " "
                + deque);
        LinkedList<String> linked = new LinkedList<>(List.of("b", "c"));
        linked.addFirst("a");
        linked.add(1, "x");
        linked.removeLastOccurrence("c");
        System.out.println("LinkedList " + linked + " " + linked.indexOf("x") + " " + linked.getLast());

        List<Integer> nums = new ArrayList<>();
        for (int i = 0; i < 12; i++) {
            nums.add((i * 7) % 12);
        }
        System.out.println("list " + nums + " sub " + nums.subList(2, 5) + " idx " + nums.indexOf(7) + " last "
                + nums.lastIndexOf(0) + " contains " + nums.contains(11));
        nums.removeIf(n -> n % 3 == 0);
        nums.replaceAll(n -> n * 10);
        System.out.println("removeIf/replaceAll " + nums);
        Collections.sort(nums);
        System.out.println("sorted " + nums + " search " + Collections.binarySearch(nums, 50) + " "
                + Collections.binarySearch(nums, 55));
        Collections.reverse(nums);
        Collections.swap(nums, 0, 1);
        Collections.rotate(nums, 2);
        System.out.println("reverse/swap/rotate " + nums + " max " + Collections.max(nums) + " min " + Collections.min(nums)
                + " freq " + Collections.frequency(List.of(1, 2, 1, 1), 1));
        List<Integer> shuffled = new ArrayList<>(List.of(1, 2, 3, 4, 5, 6, 7, 8, 9, 10));
        Collections.shuffle(shuffled, new Random(42));
        System.out.println("shuffle(42) " + shuffled);
        System.out.println("nCopies " + Collections.nCopies(3, "ab") + " singleton " + Collections.singletonList(1)
                + " empty " + Collections.emptyMap() + " disjoint " + Collections.disjoint(List.of(1), List.of(2)));
        List<String> unmodifiable = Collections.unmodifiableList(new ArrayList<>(List.of("x")));
        try {
            unmodifiable.add("y");
        } catch (UnsupportedOperationException e) {
            System.out.println("unmodifiable add threw");
        }
        Iterator<Integer> it = nums.iterator();
        while (it.hasNext()) {
            if (it.next() > 50) {
                it.remove();
            }
        }
        System.out.println("iterator remove " + nums);
        try {
            for (Integer n : nums) {
                if (n == 20) {
                    nums.add(1);
                }
            }
        } catch (ConcurrentModificationException e) {
            System.out.println("ConcurrentModificationException");
        }
        ListIterator<Integer> li = nums.listIterator();
        while (li.hasNext()) {
            int v = li.next();
            if (v == 10) {
                li.set(11);
                li.add(12);
            }
        }
        System.out.println("listIterator " + nums + " previous " + li.previous());

        int[] arr = {5, 2, 9, 1, 7};
        int[] copy = Arrays.copyOf(arr, 7);
        Arrays.sort(arr);
        System.out.println("arrays " + Arrays.toString(arr) + " " + Arrays.toString(copy) + " "
                + Arrays.binarySearch(arr, 7) + " " + Arrays.equals(arr, new int[] {1, 2, 5, 7, 9}) + " "
                + Arrays.hashCode(arr) + " " + Arrays.toString(Arrays.copyOfRange(arr, 1, 3)) + " "
                + Arrays.deepToString(new int[][] {{1}, {2, 3}}) + " " + Arrays.stream(arr).sum());
        String[] strs = {"pear", "Apple", "fig", "banana"};
        Arrays.sort(strs);
        System.out.println(Arrays.toString(strs) + " " + Arrays.asList(strs).hashCode());
        Arrays.sort(strs, String.CASE_INSENSITIVE_ORDER);
        Integer[] boxed = {3, 1, 2};
        Arrays.sort(boxed, Collections.reverseOrder());
        long[] filled = new long[4];
        Arrays.fill(filled, 7L);
        Arrays.fill(filled, 1, 3, -1L);
        System.out.println(Arrays.toString(strs) + " " + Arrays.toString(boxed) + " " + Arrays.toString(filled) + " "
                + Arrays.compare(new int[] {1, 2}, new int[] {1, 3}) + " " + Arrays.mismatch(new int[] {1, 2}, new int[] {1, 3}));
        double[] ds = {3.5, -0.0, 0.0, Double.NaN, -1};
        Arrays.sort(ds);
        System.out.println(Arrays.toString(ds));

        System.out.println("hashCodes " + List.of(1, 2, 3).hashCode() + " " + Set.of("a", "b").hashCode() + " "
                + Map.of("k", 1).hashCode() + " " + new ArrayList<>(List.of("x", "y")).hashCode() + " "
                + Map.entry("a", 1) + " " + Objects.hash(1, "a", null) + " " + Arrays.asList(1, null).hashCode() + " "
                + new HashMap<>(Map.of(1, 2)).equals(Map.of(1, 2)) + " " + List.of(1, 2).equals(Arrays.asList(1, 2)));
        IdentityHashMap<String, Integer> identity = new IdentityHashMap<>();
        String k1 = new String("k");
        String k2 = new String("k");
        identity.put(k1, 1);
        identity.put(k2, 2);
        System.out.println("IdentityHashMap size " + identity.size() + " get " + identity.get("k"));
        try {
            List.of(1, null);
        } catch (NullPointerException e) {
            System.out.println("List.of null threw");
        }
        Map<String, List<Integer>> multi = new TreeMap<>();
        for (int i = 0; i < 10; i++) {
            multi.computeIfAbsent(i % 2 == 0 ? "even" : "odd", k -> new ArrayList<>()).add(i);
        }
        System.out.println("multimap " + multi);
        StringBuilder entries = new StringBuilder();
        new TreeMap<>(hash).forEach((k, v) -> entries.append(k.charAt(0)).append(v));
        System.out.println("forEach " + entries);
        List<String> seq = new ArrayList<>(List.of("one", "two", "three"));
        seq.addFirst("zero");
        System.out.println("sequenced " + seq.reversed() + " " + seq.getLast() + " " + seq.removeFirst());
    }
}
