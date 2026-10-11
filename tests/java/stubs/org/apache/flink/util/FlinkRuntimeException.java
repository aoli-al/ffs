// Minimal compile-only dependency for the isolated Flink queue test.
package org.apache.flink.util; public class FlinkRuntimeException extends RuntimeException { public FlinkRuntimeException(String msg, Throwable cause) { super(msg, cause); } }
