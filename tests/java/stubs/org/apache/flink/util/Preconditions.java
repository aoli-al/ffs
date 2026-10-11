// Minimal compile-only dependency for the isolated Flink queue test.
package org.apache.flink.util; public final class Preconditions { public static void checkArgument(boolean ok, String msg) { if (!ok) throw new IllegalArgumentException(msg); } }
