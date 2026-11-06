package dev.horizonlab.conformance.lang;

import java.io.BufferedReader;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.StringReader;
import java.io.StringWriter;
import java.io.PrintWriter;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.util.Arrays;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Properties;
import java.util.TreeMap;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** What mods use around configs and packets, all in memory: regex, byte and character streams,
 * ByteBuffer, Base64, charsets, Properties, and java.time durations and instants. */
public class IoText {
    static final Pattern RESOURCE_LOCATION = Pattern.compile("^(?:([a-z0-9_.-]+):)?([a-z0-9_./-]+)$");

    public static void main(String[] args) throws IOException {
        for (String id : new String[] {"minecraft:stone", "mymod:items/ruby_sword", "plain", "Bad:Id", "a:b:c"}) {
            Matcher m = RESOURCE_LOCATION.matcher(id);
            System.out.println(id + " -> " + (m.matches() ? (m.group(1) == null ? "minecraft" : m.group(1)) + " | " + m.group(2)
                    : "invalid"));
        }
        Matcher numbers = Pattern.compile("(?<key>\\w+)=(?<value>-?\\d+)").matcher("a=1, bb=-22,ccc=333 d=x");
        StringBuilder found = new StringBuilder();
        while (numbers.find()) {
            found.append(numbers.group("key")).append(':').append(Integer.parseInt(numbers.group("value")) * 2).append('@')
                    .append(numbers.start()).append(' ');
        }
        System.out.println("named groups " + found.toString().trim() + " count " + Pattern.compile("\\d").matcher("a1b2c3")
                .results().count());
        System.out.println("replace " + "2026-11-06".replaceAll("(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1") + " "
                + Pattern.compile("o", Pattern.CASE_INSENSITIVE).matcher("fOo bOO").replaceAll("0") + " "
                + Pattern.matches("[A-Z][a-z]+", "Hello") + " " + Arrays.toString(Pattern.compile("\\s*,\\s*").split("a , b,c"))
                + " " + Pattern.quote("1+1") + " " + "1+1=2".split(Pattern.quote("+")).length + " "
                + "CamelCaseString".replaceAll("([a-z])([A-Z])", "$1_$2").toLowerCase());
        Matcher appended = Pattern.compile("\\{(\\d)\\}").matcher("Hello {0}, you have {1} items");
        StringBuilder out = new StringBuilder();
        String[] values = {"Steve", "3"};
        while (appended.find()) {
            appended.appendReplacement(out, values[Integer.parseInt(appended.group(1))]);
        }
        appended.appendTail(out);
        System.out.println("appendReplacement " + out);

        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (DataOutputStream data = new DataOutputStream(bytes)) {
            data.writeInt(0xCAFEBABE);
            data.writeShort(-2);
            data.writeLong(1L << 62);
            data.writeFloat(1.5f);
            data.writeDouble(Math.E);
            data.writeBoolean(true);
            data.writeUTF("h\u00e9llo \u20ac");
            data.writeByte(200);
        }
        byte[] encoded = bytes.toByteArray();
        System.out.println("DataOutputStream " + encoded.length + " " + HexFormat.of().formatHex(encoded));
        try (DataInputStream in = new DataInputStream(new ByteArrayInputStream(encoded))) {
            System.out.println("DataInputStream " + Integer.toHexString(in.readInt()) + " " + in.readShort() + " " + in.readLong()
                    + " " + in.readFloat() + " " + in.readDouble() + " " + in.readBoolean() + " " + in.readUTF() + " "
                    + in.readUnsignedByte() + " " + in.read());
        }
        ByteBuffer buffer = ByteBuffer.allocate(32);
        buffer.putInt(42).putLong(-1L).put((byte) 7).putDouble(0.5).putChar('Z');
        buffer.flip();
        System.out.println("ByteBuffer " + buffer.remaining() + " " + buffer.getInt() + " " + buffer.getLong() + " " + buffer.get()
                + " " + buffer.getDouble() + " " + buffer.getChar() + " " + buffer.position() + " " + buffer.hasRemaining());
        ByteBuffer little = ByteBuffer.wrap(new byte[] {1, 0, 0, 0, 0, 0, (byte) 0x80, 0x3f}).order(ByteOrder.LITTLE_ENDIAN);
        System.out.println("little endian " + little.getInt() + " " + little.getFloat() + " " + little.order());

        String text = "h\u00e9llo w\u00f6rld \u2603";
        byte[] utf8 = text.getBytes(StandardCharsets.UTF_8);
        byte[] latin = text.getBytes(StandardCharsets.ISO_8859_1);
        byte[] utf16 = "ab".getBytes(StandardCharsets.UTF_16);
        System.out.println("charsets " + utf8.length + " " + latin.length + " " + Arrays.toString(utf16) + " "
                + new String(utf8, StandardCharsets.UTF_8).equals(text) + " " + new String(latin, StandardCharsets.ISO_8859_1)
                + " " + new String(new byte[] {(byte) 0xff, 0x41}, StandardCharsets.UTF_8) + " "
                + new String(utf8, 0, 6, StandardCharsets.US_ASCII));
        String b64 = Base64.getEncoder().encodeToString(utf8);
        System.out.println("Base64 " + b64 + " " + new String(Base64.getDecoder().decode(b64), StandardCharsets.UTF_8) + " "
                + Base64.getUrlEncoder().withoutPadding().encodeToString(new byte[] {(byte) 0xfb, (byte) 0xff}) + " "
                + Base64.getMimeEncoder().encodeToString(new byte[60]).length());

        Properties props = new Properties();
        props.load(new StringReader("# comment\nname = My Mod\nenabled=true\nlimit: 64\nmulti=one \\\n   two\nunicode=\\u00e9\n"));
        System.out.println("Properties " + new TreeMap<>(props) + " " + props.getProperty("missing", "dflt") + " "
                + Boolean.parseBoolean(props.getProperty("enabled")));
        StringWriter written = new StringWriter();
        try (PrintWriter writer = new PrintWriter(written)) {
            writer.printf("%s=%d%n", "count", 5);
            writer.println(3.5);
            writer.print('c');
        }
        System.out.println("PrintWriter [" + written.toString().replace("\n", "|") + "]");
        BufferedReader reader = new BufferedReader(new StringReader("line one\nline two\n\nlast"));
        StringBuilder lines = new StringBuilder();
        String line;
        int n = 0;
        while ((line = reader.readLine()) != null) {
            lines.append(++n).append('[').append(line).append(']');
        }
        System.out.println("BufferedReader " + lines + " " + new BufferedReader(new StringReader("a\nb")).lines().count());

        Duration duration = Duration.ofSeconds(3725, 500_000_000);
        System.out.println("Duration " + duration + " " + duration.toMillis() + " " + duration.toMinutes() + " "
                + duration.plusMillis(250).getNano() + " " + Duration.ofMillis(50).multipliedBy(20).getSeconds() + " "
                + Duration.ZERO.isZero() + " " + Duration.ofHours(25).toDays() + " " + Duration.parse("PT1H30M").getSeconds()
                + " " + Duration.ofMinutes(90).compareTo(Duration.ofHours(1)));
        Instant instant = Instant.ofEpochMilli(1_700_000_000_123L);
        System.out.println("Instant " + instant + " " + instant.getEpochSecond() + " " + instant.plusSeconds(60).toEpochMilli() + " "
                + Instant.EPOCH + " " + Instant.parse("2026-11-06T10:15:30Z").getEpochSecond() + " "
                + Duration.between(Instant.EPOCH, instant).toDays());
    }
}
