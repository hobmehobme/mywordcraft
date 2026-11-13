package app.wordcraft.ime;

import android.app.Activity;
import android.content.Context;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;

import java.util.concurrent.ConcurrentLinkedQueue;

/**
 * Brings the system keyboard's text to a NativeActivity, which has no input connection of its own
 * (so a keyboard sends it only the keys that have a key code, and nothing for Arabic).
 *
 * It adds a 1 px view that takes the keyboard's input. What the keyboard types is queued as
 * operations; the application reads them with {@link #poll()}.
 *
 * Loaded from memory by the application (an InMemoryDexClassLoader), so nothing here is declared
 * in the manifest.
 */
public final class ImeBridge {
    private static final char SEPARATOR = '\u001f';

    private final Activity activity;
    private final ConcurrentLinkedQueue<String> operations = new ConcurrentLinkedQueue<String>();
    private volatile View view;
    /** Code points of the composing text already queued as typed (UI thread only). */
    private int composing = 0;

    private ImeBridge(Activity activity) {
        this.activity = activity;
    }

    /** Creates the bridge and adds its view on the UI thread. */
    public static ImeBridge create(final Activity activity) {
        final ImeBridge bridge = new ImeBridge(activity);
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                bridge.install();
            }
        });
        return bridge;
    }

    private void install() {
        View input = new InputView(activity, this);
        input.setFocusable(true);
        input.setFocusableInTouchMode(true);
        activity.addContentView(input, new FrameLayout.LayoutParams(1, 1));
        view = input;
    }

    /** Shows or hides the system keyboard. */
    public void show(final boolean visible) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                View input = view;
                if (input == null) {
                    return;
                }
                InputMethodManager manager = (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                if (manager == null) {
                    return;
                }
                if (visible) {
                    input.requestFocus();
                    manager.restartInput(input);
                    manager.showSoftInput(input, 0);
                } else {
                    manager.hideSoftInputFromWindow(input.getWindowToken(), 0);
                }
            }
        });
    }

    /**
     * The operations queued since the last call, separated by U+001F, or null when there are none.
     * "D3" deletes 3 characters before the caret, "Tabc" types abc, "E" is Enter.
     */
    public String poll() {
        StringBuilder out = null;
        String operation;
        while ((operation = operations.poll()) != null) {
            if (out == null) {
                out = new StringBuilder();
            } else {
                out.append(SEPARATOR);
            }
            out.append(operation);
        }
        return out == null ? null : out.toString();
    }

    private void type(CharSequence text) {
        if (text.length() > 0) {
            operations.add("T" + text);
        }
    }

    private void delete(int count) {
        if (count > 0) {
            operations.add("D" + count);
        }
    }

    private void enter() {
        operations.add("E");
    }

    /** Text the keyboard is still composing replaces what it composed before. */
    private void replaceComposing(CharSequence text, boolean stillComposing) {
        delete(composing);
        type(text);
        composing = stillComposing ? Character.codePointCount(text, 0, text.length()) : 0;
    }

    private void deleteBefore(int count) {
        if (composing > 0) {
            composing = Math.max(0, composing - count);
        }
        delete(count);
    }

    private static final class InputView extends View {
        private final ImeBridge bridge;

        InputView(Context context, ImeBridge bridge) {
            super(context);
            this.bridge = bridge;
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = InputType.TYPE_CLASS_TEXT
                    | InputType.TYPE_TEXT_FLAG_MULTI_LINE
                    | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN
                    | EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_ACTION_NONE;
            return new Connection(this, bridge);
        }
    }

    private static final class Connection extends BaseInputConnection {
        private final ImeBridge bridge;

        Connection(View target, ImeBridge bridge) {
            super(target, false);
            this.bridge = bridge;
        }

        @Override
        public boolean commitText(CharSequence text, int newCursorPosition) {
            bridge.replaceComposing(text, false);
            return true;
        }

        @Override
        public boolean setComposingText(CharSequence text, int newCursorPosition) {
            bridge.replaceComposing(text, true);
            return true;
        }

        @Override
        public boolean finishComposingText() {
            bridge.composing = 0;
            return true;
        }

        @Override
        public boolean deleteSurroundingText(int beforeLength, int afterLength) {
            bridge.deleteBefore(beforeLength);
            return true;
        }

        @Override
        public boolean sendKeyEvent(KeyEvent event) {
            if (event.getAction() == KeyEvent.ACTION_DOWN) {
                int code = event.getKeyCode();
                if (code == KeyEvent.KEYCODE_DEL) {
                    bridge.deleteBefore(1);
                } else if (code == KeyEvent.KEYCODE_ENTER) {
                    bridge.enter();
                } else {
                    int unicode = event.getUnicodeChar();
                    if (unicode != 0) {
                        bridge.type(new String(Character.toChars(unicode)));
                    }
                }
            }
            return true;
        }

        @Override
        public boolean performEditorAction(int editorAction) {
            bridge.enter();
            return true;
        }

        @Override
        public CharSequence getTextBeforeCursor(int length, int flags) {
            return "";
        }

        @Override
        public CharSequence getTextAfterCursor(int length, int flags) {
            return "";
        }
    }
}
