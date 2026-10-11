/** Optional FFS instrumentation for Java harnesses. */
public final class Ffs {
    static { System.loadLibrary("ffs_java"); }
    private Ffs() {}
    public static native void begin();
    public static native void end();
    public static native void check(boolean condition, String message);
    public static native void yield();
}
