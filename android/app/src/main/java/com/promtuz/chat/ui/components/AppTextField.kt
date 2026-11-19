package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.background
import com.promtuz.chat.ui.theme.Radii
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.error
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.tooling.preview.Preview
import com.promtuz.chat.R
import com.promtuz.chat.ui.animation.QueuedAnimatedNumber
import com.promtuz.chat.ui.theme.PromtuzTheme

@Immutable
data class AppTextFieldColors(
    val container: Color,
    val text: Color,
    val placeholder: Color,
    val label: Color,
    val supporting: Color,
    val cursor: Color,
    val divider: Color,
    val error: Color,
)

private val LocalAppTextFieldColors = compositionLocalOf<AppTextFieldColors?> { null }

object AppTextFieldDefaults {
    val Shape: Shape = RoundedCornerShape(Radii.field)
    val InputPadding = PaddingValues(vertical = 14.dp)

    @Composable
    fun textStyle(): TextStyle = RowTitleStyle

    @Composable
    fun colors(): AppTextFieldColors {
        LocalAppTextFieldColors.current?.let { return it }
        return AppTextFieldColors(
            container = if (LocalOnGlass.current) Promtuz.colors.glassCard else pageFieldFill(),
            text = Promtuz.colors.text,
            placeholder = Promtuz.colors.muted.copy(0.7f),
            label = Promtuz.colors.accentFill,
            supporting = Promtuz.colors.muted.copy(.85f),
            cursor = Promtuz.colors.accentFill,
            divider = Promtuz.colors.divider,
            error = Promtuz.colors.danger,
        )
    }
}

/**
 * Filled field with an optional fixed label and supporting content outside its container.
 * Placeholder and value always share the same text metrics. Labels never float.
 *
 * Use [AppTextFieldGroup] with [AppTextFieldInput] for multiple inputs in one surface.
 */
@Composable
fun AppTextField(
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    label: String? = null,
    placeholder: String? = null,
    enabled: Boolean = true,
    readOnly: Boolean = false,
    isError: Boolean = false,
    singleLine: Boolean = true,
    minLines: Int = 1,
    maxLines: Int = if (singleLine) 1 else Int.MAX_VALUE,
    textStyle: TextStyle = AppTextFieldDefaults.textStyle(),
    visualTransformation: VisualTransformation = VisualTransformation.None,
    keyboardOptions: KeyboardOptions = KeyboardOptions.Default,
    keyboardActions: KeyboardActions = KeyboardActions.Default,
    interactionSource: MutableInteractionSource? = null,
    leadingIcon: (@Composable () -> Unit)? = null,
    trailingIcon: (@Composable () -> Unit)? = null,
    prefix: (@Composable () -> Unit)? = null,
    suffix: (@Composable () -> Unit)? = null,
    supportingText: (@Composable () -> Unit)? = null,
    supportingIcon: (@Composable () -> Unit)? = null,
    shape: Shape = AppTextFieldDefaults.Shape,
    colors: AppTextFieldColors = AppTextFieldDefaults.colors(),
) {
    val focus = remember { FocusRequester() }
    AppTextFieldGroup(
        modifier = modifier.pointerInput(enabled) {
            // Unconsumed taps on the label or container padding should focus the input too.
            detectTapGestures { if (enabled) focus.requestFocus() }
        },
        label = label,
        isError = isError,
        supportingText = supportingText,
        supportingIcon = supportingIcon,
        shape = shape,
        colors = colors,
    ) {
        AppTextFieldInput(
            value = value, onValueChange = onValueChange,
            modifier = Modifier.focusRequester(focus).semantics { if (label != null) contentDescription = label },
            placeholder = placeholder, enabled = enabled, readOnly = readOnly, isError = isError,
            singleLine = singleLine, minLines = minLines, maxLines = maxLines,
            textStyle = textStyle, visualTransformation = visualTransformation,
            keyboardOptions = keyboardOptions, keyboardActions = keyboardActions,
            interactionSource = interactionSource,
            leadingIcon = leadingIcon, trailingIcon = trailingIcon, prefix = prefix, suffix = suffix,
            colors = colors,
        )
    }
}

/**
 * Shared surface for one or more inputs. The label stays inside; supporting content stays outside.
 * Slots allow new content without a growing list of mutually exclusive variant flags.
 *
 * ```
 * AppTextFieldGroup(label = "Your name") {
 *     AppTextFieldInput(firstName, onFirstNameChange, placeholder = "First name")
 *     AppTextFieldDivider()
 *     AppTextFieldInput(lastName, onLastNameChange, placeholder = "Last name")
 * }
 * ```
 * Inputs retain independent focus, keyboard configuration, validation and enabled state.
 */
@Composable
fun AppTextFieldGroup(
    modifier: Modifier = Modifier,
    label: String? = null,
    isError: Boolean = false,
    supportingText: (@Composable () -> Unit)? = null,
    supportingIcon: (@Composable () -> Unit)? = null,
    shape: Shape = AppTextFieldDefaults.Shape,
    colors: AppTextFieldColors = AppTextFieldDefaults.colors(),
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(modifier.fillMaxWidth()) {
        Column(Modifier.fillMaxWidth().clip(shape).background(colors.container).padding(horizontal = 16.dp)) {
            if (label != null) Text(
                label,
                modifier = Modifier.padding(top = 12.dp),
                style = SectionHeaderStyle,
                color = if (isError) colors.error else colors.label,
            )
            CompositionLocalProvider(LocalAppTextFieldColors provides colors) { content() }
        }
        if (supportingText != null) {
            val color = if (isError) colors.error else colors.supporting
            CompositionLocalProvider(LocalContentColor provides color) {
                ProvideTextStyle(FootnoteStyle.copy(color = color)) {
                    Row(
                        Modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 8.dp),
                        verticalAlignment = Alignment.Top,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        if (supportingIcon != null) Box(Modifier.size(16.dp), contentAlignment = Alignment.Center) {
                            supportingIcon()
                        }
                        Box(Modifier.weight(1f)) { supportingText() }
                    }
                }
            }
        }
    }
}

/** Editable row without a surface, for use inside [AppTextFieldGroup]. */
@Composable
fun AppTextFieldInput(
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    placeholder: String? = null,
    enabled: Boolean = true,
    readOnly: Boolean = false,
    isError: Boolean = false,
    singleLine: Boolean = true,
    minLines: Int = 1,
    maxLines: Int = if (singleLine) 1 else Int.MAX_VALUE,
    textStyle: TextStyle = AppTextFieldDefaults.textStyle(),
    visualTransformation: VisualTransformation = VisualTransformation.None,
    keyboardOptions: KeyboardOptions = KeyboardOptions.Default,
    keyboardActions: KeyboardActions = KeyboardActions.Default,
    interactionSource: MutableInteractionSource? = null,
    leadingIcon: (@Composable () -> Unit)? = null,
    trailingIcon: (@Composable () -> Unit)? = null,
    prefix: (@Composable () -> Unit)? = null,
    suffix: (@Composable () -> Unit)? = null,
    colors: AppTextFieldColors = AppTextFieldDefaults.colors(),
    contentPadding: PaddingValues = AppTextFieldDefaults.InputPadding,
) {
    val style = textStyle.copy(color = if (textStyle.color == Color.Unspecified) colors.text else textStyle.color)
    val direction = LocalLayoutDirection.current
    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        enabled = enabled,
        readOnly = readOnly,
        singleLine = singleLine,
        minLines = minLines,
        maxLines = maxLines,
        textStyle = style,
        visualTransformation = visualTransformation,
        keyboardOptions = keyboardOptions,
        keyboardActions = keyboardActions,
        interactionSource = interactionSource,
        cursorBrush = SolidColor(if (isError) colors.error else colors.cursor),
        modifier = modifier.fillMaxWidth().heightIn(min = 48.dp).semantics {
            if (isError) error("Invalid input")
        },
        decorationBox = { inner ->
            CompositionLocalProvider(LocalContentColor provides colors.supporting) {
                ProvideTextStyle(style) {
                    Row(
                        Modifier.fillMaxWidth().alpha(if (enabled) 1f else 0.38f).padding(
                            start = contentPadding.calculateStartPadding(direction),
                            end = contentPadding.calculateEndPadding(direction),
                        ),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        leadingIcon?.invoke()
                        prefix?.invoke()
                        // Pad the text, not the action's existing touch target. Padding a 48dp
                        // trailing button as well makes real fields much taller than plain previews.
                        Box(Modifier.weight(1f).padding(
                            top = contentPadding.calculateTopPadding(),
                            bottom = contentPadding.calculateBottomPadding(),
                        ), contentAlignment = if (singleLine) Alignment.CenterStart else Alignment.TopStart) {
                            if (value.isEmpty() && placeholder != null) Text(
                                placeholder, style = style, color = colors.placeholder,
                                maxLines = if (singleLine) 1 else maxLines,
                            )
                            inner()
                        }
                        if (suffix != null) {
                            Box(if (singleLine) Modifier else Modifier.align(Alignment.Bottom)
                                .padding(bottom = contentPadding.calculateBottomPadding())) { suffix() }
                        }
                        trailingIcon?.invoke()
                    }
                }
            }
        },
    )
}

/** Non-interactive remaining-character count, with a fixed width and a bounded rolling sweep. */
@Composable
fun AppTextFieldCounter(count: Int, limit: Int, modifier: Modifier = Modifier) {
    val style = MaterialTheme.typography.labelMedium.copy(fontFeatureSettings = "tnum")
    val color = if (count > limit) Promtuz.colors.danger else LocalContentColor.current
    val measurer = rememberTextMeasurer()
    val remaining = limit - count
    val digits = maxOf(remaining.toString().length, limit.toString().length)
    val numberWidthPx = remember(measurer, digits, style) {
        measurer.measure("0".repeat(digits), style).size.width
    }
    val numberWidth = with(LocalDensity.current) { numberWidthPx.toDp() }
    Row(modifier.clearAndSetSemantics { contentDescription = "$remaining characters remaining" },
        verticalAlignment = Alignment.CenterVertically) {
        QueuedAnimatedNumber(remaining, modifier = Modifier.width(numberWidth),
            contentAlignment = Alignment.CenterEnd, label = "field character count",
            style = style, color = color)
    }
}

/** Divider inset by the group's shared horizontal padding. */
@Composable
fun AppTextFieldDivider(
    modifier: Modifier = Modifier,
    colors: AppTextFieldColors = AppTextFieldDefaults.colors(),
) {
    HorizontalDivider(modifier, color = colors.divider)
}

@Preview(name = "Text field variants", showBackground = true)
@Composable
private fun AppTextFieldPreview() {
    PromtuzTheme(darkTheme = true) {
        Surface {
            var message by remember { mutableStateOf("") }
            var nickname by remember { mutableStateOf("") }
            var firstName by remember { mutableStateOf("") }
            var lastName by remember { mutableStateOf("") }
            var code by remember { mutableStateOf("abc@123!") }
            Column(Modifier.width(360.dp).padding(16.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
                AppTextField(message, { message = it }, placeholder = "Message")
                AppTextField(nickname, { nickname = it }, label = "Nickname", placeholder = "John Doe")
                AppTextFieldGroup(label = "Your name") {
                    AppTextFieldInput(firstName, { firstName = it }, placeholder = "First name")
                    AppTextFieldDivider()
                    AppTextFieldInput(lastName, { lastName = it }, placeholder = "Last name")
                }
                AppTextField(code, { code = it }, label = "Code",
                    supportingIcon = { DrawableIcon(R.drawable.oi_info, size = 16.dp, tint = LocalContentColor.current) },
                    supportingText = { Text("Notifications will be disabled") })
            }
        }
    }
}
