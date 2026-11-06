package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.IntSummaryStatistics;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalDouble;
import java.util.OptionalInt;
import java.util.TreeMap;
import java.util.function.Function;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.LongStream;
import java.util.stream.Stream;

/** Streams and collectors (sequential), Optional and Objects. */
public class Streams {
    record Item(String name, String kind, int price) {}

    public static void main(String[] args) {
        List<Item> items = List.of(new Item("apple", "fruit", 3), new Item("leek", "veg", 2), new Item("pear", "fruit", 4),
                new Item("kale", "veg", 5), new Item("fig", "fruit", 7), new Item("bread", "bakery", 3));
        System.out.println(items.stream().filter(i -> i.price() > 2).map(Item::name).collect(Collectors.toList()));
        System.out.println(items.stream().map(Item::name).sorted().collect(Collectors.joining(", ", "[", "]")));
        System.out.println(items.stream().mapToInt(Item::price).sum() + " " + items.stream().mapToInt(Item::price).max().getAsInt()
                + " " + items.stream().mapToInt(Item::price).average().getAsDouble() + " "
                + items.stream().map(Item::price).reduce(0, Integer::sum) + " "
                + items.stream().map(Item::price).reduce(Integer::max).get());
        Map<String, List<String>> byKind = items.stream().collect(Collectors.groupingBy(Item::kind,
                Collectors.mapping(Item::name, Collectors.toList())));
        System.out.println("groupingBy " + byKind);
        System.out.println("groupingBy tree " + items.stream().collect(Collectors.groupingBy(Item::kind, TreeMap::new,
                Collectors.summingInt(Item::price))));
        System.out.println("counting " + items.stream().collect(Collectors.groupingBy(Item::kind, Collectors.counting())));
        System.out.println("partitioning " + items.stream().collect(Collectors.partitioningBy(i -> i.price() % 2 == 0,
                Collectors.mapping(Item::name, Collectors.joining("/")))));
        System.out.println("toMap " + items.stream().collect(Collectors.toMap(Item::name, Item::price)));
        System.out.println("toMap merge " + items.stream().collect(Collectors.toMap(Item::kind, Item::price, Integer::sum,
                TreeMap::new)));
        System.out.println("toSet " + items.stream().map(Item::kind).collect(Collectors.toSet()));
        System.out.println("averaging " + items.stream().collect(Collectors.averagingInt(Item::price)) + " "
                + items.stream().collect(Collectors.averagingDouble(i -> i.price() / 3.0)));
        IntSummaryStatistics stats = items.stream().collect(Collectors.summarizingInt(Item::price));
        System.out.println("stats " + stats);
        System.out.println("minBy " + items.stream().collect(Collectors.minBy(Comparator.comparingInt(Item::price))).get()
                + " collectingAndThen " + items.stream().map(Item::name).collect(Collectors.collectingAndThen(
                        Collectors.toList(), List::size)) + " teeing " + items.stream().collect(Collectors.teeing(
                                Collectors.counting(), Collectors.summingInt(Item::price), (n, s) -> s + "/" + n)));
        System.out.println("flatMap " + Stream.of("a,b", "c", "d,e,f").flatMap(s -> Arrays.stream(s.split(",")))
                .collect(Collectors.toList()) + " distinct " + Stream.of(3, 1, 3, 2, 1).distinct().toList() + " limit/skip "
                + IntStream.range(0, 20).skip(5).limit(4).boxed().toList());
        System.out.println("iterate " + Stream.iterate(1, x -> x * 3).limit(8).toList() + " "
                + Stream.iterate(1, x -> x < 100, x -> x * 2).toList() + " generate "
                + Stream.generate(() -> "z").limit(3).collect(Collectors.joining()));
        System.out.println("matches " + items.stream().anyMatch(i -> i.price() > 6) + " " + items.stream().allMatch(i -> i.price() > 1)
                + " " + items.stream().noneMatch(i -> i.name().isEmpty()) + " findFirst "
                + items.stream().filter(i -> i.kind().equals("veg")).findFirst().map(Item::name).orElse("none"));
        System.out.println("rangeClosed " + IntStream.rangeClosed(1, 5).map(x -> x * x).sum() + " "
                + IntStream.of(3, 1, 2).sorted().mapToObj(Integer::toString).collect(Collectors.joining("<")) + " "
                + LongStream.rangeClosed(1, 20).reduce(1, (a, b) -> a * b) + " "
                + Arrays.toString(IntStream.range(0, 5).map(i -> 4 - i).toArray()) + " "
                + IntStream.range(0, 10).filter(i -> i % 3 == 0).count() + " " + IntStream.empty().max());
        List<String> peeked = new ArrayList<>();
        long count = Stream.of("x", "y", "z").peek(peeked::add).map(String::toUpperCase).count();
        System.out.println("peek " + peeked + " count " + count);
        List<String> order = new ArrayList<>();
        Stream.of(1, 2, 3, 4).filter(i -> {
            order.add("f" + i);
            return i % 2 == 0;
        }).map(i -> {
            order.add("m" + i);
            return i;
        }).findFirst();
        System.out.println("laziness " + order);
        System.out.println("sorted comparator " + items.stream().sorted(Comparator.comparing(Item::kind)
                .thenComparing(Item::price, Comparator.reverseOrder())).map(Item::name).toList());
        System.out.println("max " + items.stream().max(Comparator.comparingInt(Item::price)).get().name() + " min "
                + Stream.of("bb", "a", "ccc").min(Comparator.naturalOrder()).get() + " toArray "
                + Arrays.toString(Stream.of("q", "r").toArray(String[]::new)));
        System.out.println("chars " + "hello world".chars().filter(c -> c == 'o').count() + " "
                + "abc".chars().mapToObj(c -> String.valueOf((char) (c + 1))).collect(Collectors.joining()) + " "
                + Stream.of("b", "a", "c").collect(Collectors.toCollection(java.util.TreeSet::new)));
        Map<Boolean, Long> parity = IntStream.range(0, 7).boxed().collect(Collectors.partitioningBy(i -> i % 2 == 0,
                Collectors.counting()));
        System.out.println("parity " + parity + " identity map " + Stream.of("x", "yy").collect(Collectors.toMap(
                Function.identity(), String::length)));
        try {
            Stream.of("a", "a").collect(Collectors.toMap(Function.identity(), String::length));
        } catch (IllegalStateException e) {
            System.out.println("duplicate key: " + e.getMessage());
        }
        Stream<String> once = Stream.of("x");
        once.count();
        try {
            once.count();
        } catch (IllegalStateException e) {
            System.out.println("reused stream: " + e.getMessage());
        }

        Optional<String> some = Optional.of("value");
        Optional<String> none = Optional.empty();
        System.out.println(some + " " + none + " " + some.map(String::length).orElse(-1) + " " + none.map(String::length)
                .orElse(-1) + " " + some.filter(s -> s.startsWith("x")).isPresent() + " " + none.or(() -> Optional.of("alt"))
                .get() + " " + some.flatMap(s -> Optional.of(s + "!")).get() + " " + none.orElseGet(() -> "gen") + " "
                + Optional.ofNullable(null).isEmpty() + " " + some.stream().count() + " " + some.equals(Optional.of("value"))
                + " " + some.hashCode() + " " + none.hashCode());
        some.ifPresent(v -> System.out.println("ifPresent " + v));
        none.ifPresentOrElse(v -> System.out.println("no"), () -> System.out.println("ifPresentOrElse empty"));
        try {
            none.orElseThrow();
        } catch (java.util.NoSuchElementException e) {
            System.out.println("orElseThrow: " + e.getMessage());
        }
        try {
            none.orElseThrow(() -> new IllegalArgumentException("custom"));
        } catch (IllegalArgumentException e) {
            System.out.println("orElseThrow supplier: " + e.getMessage());
        }
        try {
            Optional.of(null);
        } catch (NullPointerException e) {
            System.out.println("Optional.of(null) threw");
        }
        System.out.println(OptionalInt.of(5) + " " + OptionalInt.empty() + " " + OptionalDouble.of(2.5).getAsDouble() + " "
                + OptionalInt.empty().orElse(9));

        System.out.println("Objects " + Objects.equals(null, null) + " " + Objects.equals("a", null) + " "
                + Objects.hashCode(null) + " " + Objects.hash("a", 1) + " " + Objects.toString(null, "dflt") + " "
                + Objects.isNull(null) + " " + Objects.nonNull(1) + " " + Objects.requireNonNullElse(null, "else") + " "
                + Objects.requireNonNullElseGet(null, () -> "get") + " " + Objects.compare("a", "b", Comparator.naturalOrder())
                + " " + Objects.checkIndex(2, 3) + " " + Objects.deepEquals(new int[] {1}, new int[] {1}) + " "
                + Objects.toString(5));
        try {
            Objects.requireNonNull(null, "thing must not be null");
        } catch (NullPointerException e) {
            System.out.println("requireNonNull: " + e.getMessage());
        }
        try {
            Objects.checkIndex(5, 3);
        } catch (IndexOutOfBoundsException e) {
            System.out.println("checkIndex: " + e.getMessage());
        }
    }
}
