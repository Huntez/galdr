package io.github.huntez.galdr

import android.os.Handler
import android.os.Looper

/** What the service is doing, for the UI. Written from any thread, listeners run on main. */
object Status {
    enum class Phase { STOPPED, WAITING, STREAMING }

    @Volatile var phase = Phase.STOPPED
        private set
    @Volatile var peer: String? = null
        private set
    @Volatile var error: String? = null
        private set
    /** Peak of the last captured block, 0..1. */
    @Volatile var level = 0f

    private val main = Handler(Looper.getMainLooper())
    private val listeners = mutableListOf<() -> Unit>()

    fun set(phase: Phase, peer: String? = null, error: String? = null) {
        this.phase = phase
        this.peer = peer
        this.error = error
        if (phase != Phase.STREAMING) level = 0f
        main.post { listeners.toList().forEach { it() } }
    }

    /** Reports an error without changing the phase. */
    fun fail(message: String) = set(phase, peer, message)

    /** Main thread only. */
    fun listen(listener: () -> Unit) {
        listeners += listener
    }

    fun unlisten(listener: () -> Unit) {
        listeners -= listener
    }
}
