# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# --- uniffi generated bindings + JNA (JNI/reflection, must survive R8) ---
-keep class uniffi.core.** { *; }
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { *; }
-keep class * implements com.sun.jna.Library { *; }
-dontwarn java.awt.**

# Match the startup workaround in Promtuz: retain frame-batched IME commands so focus transfers
# don't briefly hide the keyboard. Remove together after updating to Compose's upstream fix.
-assumevalues class androidx.compose.ui.AndroidComposeUiFlags {
    public static boolean isOutOfFrameSchedulerForTextInputEventsEnabled return false;
}
