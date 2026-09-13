package com.promtuz.chat.ui.components

import android.os.Build
import android.window.OnBackInvokedCallback
import android.window.OnBackInvokedDispatcher
import androidx.activity.compose.BackHandler
import androidx.compose.runtime.*
import androidx.compose.ui.platform.LocalView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner

/** Search is an overlay: Back closes the mode, including when the IME is open. */
@Composable
internal fun PickerSearchBackHandler(enabled: Boolean, onBack: () -> Unit) {
    BackHandler(enabled, onBack)
    if (Build.VERSION.SDK_INT >= 33) {
        val view = LocalView.current
        val lifecycle = LocalLifecycleOwner.current.lifecycle
        val currentBack by rememberUpdatedState(onBack)
        DisposableEffect(view, lifecycle, enabled) {
            val dispatcher = view.findOnBackInvokedDispatcher()
            val callback = OnBackInvokedCallback { currentBack() }
            var registered = false
            fun update() {
                val active = enabled && lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)
                if (active && !registered && dispatcher != null) {
                    dispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_OVERLAY, callback)
                    registered = true
                } else if (!active && registered) {
                    dispatcher?.unregisterOnBackInvokedCallback(callback)
                    registered = false
                }
            }
            val observer = LifecycleEventObserver { _, _ -> update() }
            lifecycle.addObserver(observer)
            update()
            onDispose {
                lifecycle.removeObserver(observer)
                if (registered) dispatcher?.unregisterOnBackInvokedCallback(callback)
            }
        }
    }
}
