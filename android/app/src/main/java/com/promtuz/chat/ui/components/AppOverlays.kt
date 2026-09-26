package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogWindowProvider
import androidx.compose.ui.semantics.paneTitle
import androidx.compose.ui.semantics.semantics
import com.promtuz.chat.R
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** Opaque modal surface, independent of the screen behind it. */
@Composable
fun Modifier.modalSurface(shape: Shape): Modifier =
    clip(shape).background(MaterialTheme.colorScheme.surfaceContainer)

/** Window motion also runs when a caller removes its dialog immediately after an action. */
@Composable
internal fun ModalWindowMotion() {
    val view = LocalView.current
    SideEffect {
        (view.parent as? DialogWindowProvider)?.window?.setWindowAnimations(R.style.AppModalAnimation)
    }
}

/** Native dialog focus, outside touch and Back behavior, with fade/scale window motion. */
@Composable
internal fun AppModal(
    onDismissRequest: () -> Unit,
    content: @Composable () -> Unit,
) {
    Dialog(onDismissRequest) {
        ModalWindowMotion()
        Box(Modifier.sizeIn(minWidth = 280.dp, maxWidth = 560.dp)
            .semantics { paneTitle = "Dialog" }, propagateMinConstraints = true) { content() }
    }
}

/** Material button ordering/wrapping; only the footer's outer padding is reduced. */
@Composable
internal fun ModalActions(confirmButton: @Composable () -> Unit, dismissButton: (@Composable () -> Unit)?) {
    val direction = LocalLayoutDirection.current
    Box(Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 4.dp), contentAlignment = Alignment.CenterEnd) {
        CompositionLocalProvider(LocalLayoutDirection provides
            if (direction == LayoutDirection.Ltr) LayoutDirection.Rtl else LayoutDirection.Ltr) {
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                CompositionLocalProvider(LocalLayoutDirection provides direction) {
                    confirmButton()
                    dismissButton?.invoke()
                }
            }
        }
    }
}

@Composable
fun AppAlertDialog(
    onDismissRequest: () -> Unit,
    confirmButton: @Composable () -> Unit,
    modifier: Modifier = Modifier,
    dismissButton: (@Composable () -> Unit)? = null,
    icon: (@Composable () -> Unit)? = null,
    title: (@Composable () -> Unit)? = null,
    text: (@Composable () -> Unit)? = null,
) {
    AppModal(onDismissRequest) {
        Column(modifier.modalSurface(MaterialTheme.shapes.extraLarge)) {
            Column(Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState())
                .padding(start = 24.dp, end = 24.dp, top = 24.dp)) {
                icon?.let {
                    CompositionLocalProvider(LocalContentColor provides AlertDialogDefaults.iconContentColor) {
                        Box(Modifier.align(Alignment.CenterHorizontally).padding(bottom = 12.dp)) { it() }
                    }
                }
                title?.let {
                    CompositionLocalProvider(LocalContentColor provides AlertDialogDefaults.titleContentColor) {
                        ProvideTextStyle(MaterialTheme.typography.titleLargeEmphasized.copy(fontWeight = FontWeight.Medium)) {
                            Box(Modifier.align(if (icon == null) Alignment.Start else Alignment.CenterHorizontally)
                                .padding(bottom = 12.dp)) { it() }
                        }
                    }
                }
                text?.let {
                    CompositionLocalProvider(LocalContentColor provides AlertDialogDefaults.titleContentColor) {
                        ProvideTextStyle(MaterialTheme.typography.bodyMediumEmphasized.copy(lineHeight = 18.5.sp)) { it() }
                    }
                }
            }
            ModalActions(confirmButton, dismissButton)
        }
    }
}
