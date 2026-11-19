package com.promtuz.chat.ui.appearance

import android.content.Context
import android.content.SharedPreferences
import android.os.Handler
import android.os.Looper
import androidx.core.content.edit
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.serialization.json.Json

/** One JSON blob in prefs; unknown keys are ignored so a stored preset survives field changes. */
object AppearanceStore {
    private const val KEY = "chat_appearance"
    private lateinit var prefs: SharedPreferences
    private val json = Json { ignoreUnknownKeys = true }

    private val state = MutableStateFlow(ChatAppearance.Default)
    val appearance: StateFlow<ChatAppearance> get() = state

    fun init(context: Context) {
        prefs = context.getSharedPreferences("appearance", Context.MODE_PRIVATE)
        state.value = prefs.getString(KEY, null)
            ?.let { runCatching { json.decodeFromString<ChatAppearance>(it) }.getOrNull() }
            ?: ChatAppearance.Default
    }

    private val handler = Handler(Looper.getMainLooper())
    private val save = Runnable {
        prefs.edit { putString(KEY, json.encodeToString(ChatAppearance.serializer(), state.value)) }
    }

    /** Applies at once; the write waits for a pause, since a slider drag updates every frame. */
    fun update(transform: (ChatAppearance) -> ChatAppearance) {
        state.value = transform(state.value)
        handler.removeCallbacks(save)
        handler.postDelayed(save, 300)
    }
}
