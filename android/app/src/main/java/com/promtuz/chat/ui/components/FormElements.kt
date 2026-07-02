package com.promtuz.chat.ui.components

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.ui.*
import androidx.compose.ui.graphics.*
import androidx.compose.ui.text.*
import androidx.compose.ui.text.font.*
import androidx.compose.ui.text.input.*
import androidx.compose.ui.tooling.preview.*
import androidx.compose.ui.unit.*
import com.promtuz.chat.R
import com.promtuz.chat.ui.constants.CHECKBOX
import com.promtuz.chat.ui.constants.CORNER_RADIUS_RATIO
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.theme.PromtuzTheme

object OutlinedFormElements {
    @Composable
    fun TextField(
        modifier: Modifier = Modifier,
        value: String,
        onValueChange: (String) -> Unit,
        label: String? = null,
        placeholder: String? = null,
        leadingIcon: @Composable (() -> Unit)? = null,
        trailingIcon: @Composable (() -> Unit)? = null,
        textStyle: TextStyle = LocalTextStyle.current,
        prefix: @Composable (() -> Unit)? = null,
        suffix: @Composable (() -> Unit)? = null,
        supportingText: @Composable (() -> Unit)? = null,
        isError: Boolean = false,
        enabled: Boolean = true,
        readOnly: Boolean = false,
        visualTransformation: VisualTransformation = VisualTransformation.None,
        keyboardOptions: KeyboardOptions = KeyboardOptions.Default,
        keyboardActions: KeyboardActions = KeyboardActions.Default,
        interactionSource: MutableInteractionSource? = null,
    ) {
        val colors = MaterialTheme.colorScheme

        OutlinedTextField(
            modifier = modifier
                .fillMaxWidth(),
            value = value,
            onValueChange = onValueChange,
            label = label?.let {
                {
                    Text(
                        label,
                        fontWeight = FontWeight.W500,
                    )
                }
            },
            textStyle = textStyle,
            placeholder = placeholder?.let {
                {
                    Text(
                        placeholder,
                        fontWeight = FontWeight.W500
                    )
                }
            },
            leadingIcon = leadingIcon,
            trailingIcon = trailingIcon,
            prefix = prefix,
            suffix = suffix,
            supportingText = supportingText,
            isError = isError,
            enabled = enabled,
            readOnly = readOnly,
            visualTransformation = visualTransformation,
            keyboardOptions = keyboardOptions,
            keyboardActions = keyboardActions,
            interactionSource = interactionSource,
//            colors = OutlinedTextFieldDefaults.colors(
//                focusedLabelColor = colors.focused.label,
//                focusedBorderColor = colors.focused.border,
//                focusedPlaceholderColor = colors.focused.placeholder,
//
//                unfocusedLabelColor = colors.unfocused.label,
//                unfocusedBorderColor = colors.unfocused.border,
//                unfocusedPlaceholderColor = colors.unfocused.placeholder,
//
//                errorLabelColor = colors.error.label,
//                errorBorderColor = colors.error.border,
//                errorPlaceholderColor = colors.error.placeholder,
//
//                disabledLabelColor = colors.disabled.label,
//                disabledBorderColor = colors.disabled.border,
//                disabledPlaceholderColor = colors.disabled.placeholder
//            ),
            shape = RoundedCornerShape(16.dp),
            singleLine = true
        )
    }


    @Composable
    fun Checkbox(
        checked: Boolean,
        onCheckedChange: ((Boolean) -> Unit)?,
        modifier: Modifier = Modifier,
        enabled: Boolean = true
    ) {
        val colors = MaterialTheme.colorScheme

        val borderColor by animateColorAsState(
            if (checked) Color.Transparent else colors.onSurfaceVariant,
            Tweens.microInteraction()
        )

        val backgroundColor by animateColorAsState(
            if (enabled) if (checked) MaterialTheme.colorScheme.primary else Color.Transparent else colors.onSurfaceVariant.copy(
                0.75f
            ),
            Tweens.microInteraction()
        )

        val checkBoxSize = 22.dp


        val checkColor by animateColorAsState(
            if (checked) MaterialTheme.colorScheme.background else borderColor,
            Tweens.microInteraction()
        )

        CompositionLocalProvider(LocalMinimumInteractiveComponentSize provides Dp.Unspecified) {
            Card(
                modifier = modifier.size(checkBoxSize),
                elevation = CardDefaults.cardElevation(0.dp, 0.dp, 0.dp, 0.dp, 0.dp, 0.dp),
                border = BorderStroke(1.5.dp, borderColor),
                shape = RoundedCornerShape(checkBoxSize * CORNER_RADIUS_RATIO),
                colors = CardDefaults.cardColors(Color.Transparent),
                onClick = {
                    onCheckedChange?.invoke(!checked)
                }
            ) {
                AnimatedVisibility(
                    checked,
                    enter = fadeIn(Tweens.microInteraction()),
                    exit = fadeOut(Tweens.microInteraction())
                ) {
                    Box(
                        Modifier
                            .background(backgroundColor)
                            .fillMaxSize(),
                        contentAlignment = Alignment.Center
                    ) {
                        DrawableIcon(
                            R.drawable.i_check,
                            Modifier.size(checkBoxSize * CHECKBOX.ICON_SIZE_RATIO),
                            "Checked",
                            checkColor
                        )
                    }
                }
            }
        }
    }
}


@Preview
@Composable
fun CheckboxPreview(modifier: Modifier = Modifier) {
    PromtuzTheme(true) {
        Column(
            Modifier
                .background(MaterialTheme.colorScheme.background)
                .padding(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            OutlinedFormElements.Checkbox(
                checked = false,
                onCheckedChange = null
            )

            OutlinedFormElements.Checkbox(
                checked = true,
                onCheckedChange = null
            )
        }
    }
}