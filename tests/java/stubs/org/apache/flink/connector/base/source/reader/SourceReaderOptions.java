// Minimal compile-only dependency for the isolated Flink queue test.
package org.apache.flink.connector.base.source.reader; public final class SourceReaderOptions { public static final Option ELEMENT_QUEUE_CAPACITY = new Option(); public static final class Option { public int defaultValue() { return 2; } } }
