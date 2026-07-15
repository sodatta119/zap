package com.zap.transfer

/**
 * Kotlin side of the JNI bridge into the Rust `zap-android` library.
 *
 * The three `external` functions map to the exported symbols in
 * `crates/zap-android/src/lib.rs`. The library name "zap_android" matches the
 * `.so` files bundled under `src/main/jniLibs/<abi>/libzap_android.so`.
 */
object NativeBridge {
    init {
        System.loadLibrary("zap_android")
    }

    /**
     * Start the web server sharing [dir] on [port], bound to all interfaces.
     * Pass [user] and [pass] to require a login (HTTP Basic auth), or null for
     * none. Returns an opaque handle, or 0 if the server could not be started.
     */
    external fun nativeStart(dir: String, port: Int, user: String?, pass: String?): Long

    /** The URL another device on the same Wi-Fi should open, or null. */
    external fun nativeUrl(handle: Long): String?

    /** Stop the server and release the handle. Safe to call with 0. */
    external fun nativeStop(handle: Long)
}

/**
 * Tiny in-process holder so the [MainActivity] UI can reflect what
 * [ZapService] is doing. Both run in the same process, so a shared object is
 * enough — no IPC needed.
 */
object ZapState {
    @Volatile
    var url: String? = null
        private set

    @Volatile
    var running: Boolean = false
        private set

    /** Set by the activity to be notified when [update] is called. */
    var onChange: (() -> Unit)? = null

    fun update(url: String?, running: Boolean) {
        this.url = url
        this.running = running
        onChange?.invoke()
    }
}
