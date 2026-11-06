package dev.horizonlab.conformance.lang;

import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;
import java.util.Arrays;
import java.util.Random;
import java.util.SplittableRandom;
import java.util.UUID;

/** Math and StrictMath to the last bit (Math's intrinsics on HotSpot included), java.util.Random's
 * sequences, UUID, and the BigInteger/BigDecimal operations mods reach for. */
public class MathRandom {
    static final double[] ARGS = {0.0, -0.0, 0.5, 1.0, -1.0, 2.0, Math.PI / 3, 10.0, 1e-10, 123.456, -7.25, 1e10, 709.0,
            Double.NaN, Double.POSITIVE_INFINITY};

    static String bits(double v) {
        return Double.isNaN(v) ? "NaN" : Long.toHexString(Double.doubleToRawLongBits(v));
    }

    public static void main(String[] args) {
        for (double x : ARGS) {
            System.out.println("x=" + x + " sin " + bits(Math.sin(x)) + " cos " + bits(Math.cos(x)) + " tan " + bits(Math.tan(x))
                    + " exp " + bits(Math.exp(x)) + " log " + bits(Math.log(x)) + " log10 " + bits(Math.log10(x)) + " sqrt "
                    + bits(Math.sqrt(x)) + " cbrt " + bits(Math.cbrt(x)) + " atan " + bits(Math.atan(x)) + " asin "
                    + bits(Math.asin(x / 16)) + " acos " + bits(Math.acos(x / 16)) + " sinh " + bits(Math.sinh(x / 8))
                    + " tanh " + bits(Math.tanh(x)) + " expm1 " + bits(Math.expm1(x / 4)) + " log1p " + bits(Math.log1p(x)));
            System.out.println("   strict sin " + bits(StrictMath.sin(x)) + " cos " + bits(StrictMath.cos(x)) + " exp "
                    + bits(StrictMath.exp(x)) + " log " + bits(StrictMath.log(x)) + " pow " + bits(StrictMath.pow(x, 1.7))
                    + " atan2 " + bits(StrictMath.atan2(x, 3)) + " hypot " + bits(StrictMath.hypot(x, 4)));
        }
        double[][] pows = {{2, 10}, {2, 0.5}, {-8, 1.0 / 3}, {10, -3}, {0, 0}, {1.0001, 10000}, {-2, 3}, {Double.NaN, 0},
                {1, Double.POSITIVE_INFINITY}};
        for (double[] p : pows) {
            System.out.print(" pow(" + p[0] + "," + p[1] + ")=" + Math.pow(p[0], p[1]));
        }
        System.out.println();
        System.out.println("atan2 " + Math.atan2(1, 1) + " " + Math.atan2(-0.0, -1) + " hypot " + Math.hypot(3, 4) + " "
                + Math.hypot(1e300, 1e300) + " toRadians " + Math.toRadians(180) + " toDegrees " + Math.toDegrees(1));
        double[] rounding = {2.5, -2.5, 3.5, -0.5, 0.49999999999999994, 1e16 + 1, Double.NaN, -0.0, 7.0000001};
        for (double r : rounding) {
            System.out.println("round " + r + ": round=" + Math.round(r) + " rint=" + Math.rint(r) + " floor=" + Math.floor(r)
                    + " ceil=" + Math.ceil(r) + " roundf=" + Math.round((float) r) + " signum=" + Math.signum(r) + " abs="
                    + Math.abs(r));
        }
        System.out.println("fma " + Math.fma(0.1, 10, -1) + " IEEEremainder " + Math.IEEEremainder(10, 3) + " copySign "
                + Math.copySign(3, -0.0) + " scalb " + Math.scalb(1.5, 4) + " getExponent " + Math.getExponent(1000.0)
                + " nextUp " + Math.nextUp(1.0) + " nextAfter " + Math.nextAfter(1f, 0) + " ulp " + Math.ulp(1e10));
        try {
            Math.addExact(Integer.MAX_VALUE, 1);
        } catch (ArithmeticException e) {
            System.out.println("addExact: " + e.getMessage());
        }
        try {
            Math.toIntExact(1L << 32);
        } catch (ArithmeticException e) {
            System.out.println("toIntExact: " + e.getMessage());
        }
        System.out.println("multiplyExact " + Math.multiplyExact(46341L, 46341L) + " negateExact " + Math.negateExact(-5)
                + " clamp " + Math.clamp(15, 0, 10) + " " + Math.clamp(-2.5, -1.0, 1.0) + " absExact " + Math.absExact(-9));

        Random random = new Random(42);
        System.out.println("Random(42) " + random.nextInt() + " " + random.nextInt(100) + " " + random.nextInt(1 << 20) + " "
                + random.nextLong() + " " + random.nextDouble() + " " + random.nextFloat() + " " + random.nextBoolean() + " "
                + random.nextGaussian() + " " + random.nextGaussian() + " " + random.nextInt(5, 10) + " "
                + random.nextLong(1000L) + " " + random.nextDouble(2.0, 3.0));
        byte[] bytes = new byte[10];
        random.nextBytes(bytes);
        System.out.println("bytes " + Arrays.toString(bytes) + " ints " + Arrays.toString(new Random(7).ints(5, 0, 50).toArray())
                + " doubles " + new Random(7).doubles(2).sum());
        Random reseeded = new Random(0);
        int first = reseeded.nextInt();
        reseeded.setSeed(0);
        System.out.println("setSeed repeats " + (first == reseeded.nextInt()) + " " + first);
        long seed = 12345;
        Random worldgen = new Random(seed);
        long a = worldgen.nextLong() | 1L;
        long b = worldgen.nextLong() | 1L;
        System.out.println("worldgen-style " + a + " " + b + " " + ((16 * a + 32 * b) ^ seed));
        SplittableRandom split = new SplittableRandom(99);
        System.out.println("SplittableRandom " + split.nextInt() + " " + split.nextLong() + " " + split.split().nextInt(10));

        UUID fixed = new UUID(0x123456789abcdef0L, 0x0fedcba987654321L);
        UUID parsed = UUID.fromString("550e8400-e29b-41d4-a716-446655440000");
        UUID named = UUID.nameUUIDFromBytes("OfflinePlayer:Steve".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        System.out.println("UUID " + fixed + " " + parsed.version() + " " + parsed.variant() + " " + parsed.hashCode() + " "
                + named + " " + named.version() + " " + fixed.compareTo(parsed) + " " + parsed.getMostSignificantBits());

        BigInteger big = BigInteger.valueOf(2).pow(100).subtract(BigInteger.ONE);
        System.out.println("BigInteger " + big + " " + big.bitLength() + " " + big.mod(BigInteger.valueOf(97)) + " "
                + big.multiply(big).toString(16) + " " + new BigInteger("-123456789012345678901234567890").divide(
                        BigInteger.valueOf(7)) + " " + BigInteger.TEN.gcd(BigInteger.valueOf(4)) + " "
                + BigInteger.valueOf(17).modPow(BigInteger.valueOf(5), BigInteger.valueOf(23)) + " "
                + big.testBit(99) + " " + BigInteger.valueOf(-5).shiftRight(1) + " " + big.longValue());
        BigDecimal price = new BigDecimal("19.99");
        System.out.println("BigDecimal " + price.multiply(BigDecimal.valueOf(3)) + " "
                + price.divide(BigDecimal.valueOf(3), 5, RoundingMode.HALF_UP) + " " + price.setScale(1, RoundingMode.HALF_EVEN)
                + " " + new BigDecimal("1.10").add(new BigDecimal("2.205")) + " " + BigDecimal.valueOf(0.1).add(BigDecimal.valueOf(0.2))
                + " " + new BigDecimal(0.1).toString().substring(0, 20) + " " + new BigDecimal("1E+3").toPlainString() + " "
                + new BigDecimal("2.50").stripTrailingZeros() + " " + new BigDecimal("2.5").compareTo(new BigDecimal("2.50")) + " "
                + new BigDecimal("2.5").equals(new BigDecimal("2.50")));
    }
}
