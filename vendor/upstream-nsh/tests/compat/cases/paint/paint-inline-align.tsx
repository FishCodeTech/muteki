// A case of the API's test suite (docs/dev/compatibility.md): where
// `verticalAlign` (and its `align-*` classes) puts a view inside a text in
// its line: on the baseline (the default), at the top or the bottom of the
// text's font, or in its middle.
import { Text, View } from "nsh";

function App() {
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <Text testID="inline-default" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-default" style={{ width: 4, height: 12 }} />
      </Text>
      <Text testID="inline-baseline" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-baseline" style={{ width: 4, height: 12, verticalAlign: "baseline" }} />
      </Text>
      <Text testID="inline-top" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-top" style={{ width: 4, height: 12, verticalAlign: "text-top" }} />
      </Text>
      <Text testID="inline-bottom" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-bottom" style={{ width: 4, height: 12, verticalAlign: "text-bottom" }} />
      </Text>
      <Text testID="inline-middle" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-middle" style={{ width: 4, height: 12, verticalAlign: "middle" }} />
      </Text>
      <Text testID="inline-class" style={{ alignSelf: "flex-start" }}>
        A<View testID="icon-class" class="align-text-top" style={{ width: 4, height: 12 }} />
      </Text>
    </View>
  );
}

export default App;
