package dev.horizonlab.conformance.bench;

import java.util.List;
import java.util.Map;
import java.util.function.IntUnaryOperator;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/** Lambdas and streams: primitive pipelines, boxed pipelines with collectors, and plain lambda calls. */
public class Streams2 {
    record Entry(String group, int value) {}

    public static void main(String[] args) {
        long evens = IntStream.range(0, 300_000).filter(i -> i % 2 == 0).mapToLong(i -> (long) i * i).sum();
        List<Entry> entries = IntStream.range(0, 50_000).mapToObj(i -> new Entry("g" + (i % 13), i)).toList();
        Map<String, Integer> sums = entries.stream().collect(Collectors.groupingBy(Entry::group,
                Collectors.summingInt(Entry::value)));
        IntUnaryOperator step = x -> x * 1103515245 + 12345;
        int x = 1;
        for (int i = 0; i < 300_000; i++) {
            x = step.applyAsInt(x);
        }
        System.out.println(evens + " " + sums.get("g7") + " " + x);
    }
}
