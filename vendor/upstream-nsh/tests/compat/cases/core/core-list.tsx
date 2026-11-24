// A case of the API's test suite (docs/dev/compatibility.md): `nsh/list`'s
// `List` (`nsh:rt`'s `list`): the rows near the viewport rendered from
// `each`, with a header, a footer and separators, rows added when `each`
// changes, the padding of its content (`contentStyle`,
// `contentClass`), and what an empty list shows; `Scroll`'s view around its
// content (`contentStyle`, `contentClass`); the rows of the overscan around
// the viewport, rendered once the frames after the first bind them. Its
// script checks each.
import { Scroll, Text, View, signal } from "nsh";
import { List } from "nsh";

function App() {
  const items = signal(["Apple", "Banana", "Cherry"]);
  return (
    <View testID="root" style={{ flex: 1, paddingTop: 64 }}>
      <View testID="add" onPress={() => items.set([...items(), `Item ${items().length + 1}`])}>
        <Text>Add</Text>
      </View>
      <List
        testID="list"
        each={items()}
        style={{ height: 300 }}
        contentStyle={{ paddingHorizontal: 10 }}
        header={<Text>Fruits</Text>}
        footer={() => <Text>End of fruits</Text>}
        separator={() => <View testID="sep" style={{ height: 2 }} />}
      >
        {(item, index) => (
          <View testID={`row-${index()}`} style={{ height: 40 }}>
            <Text>
              {index() + 1}. {item()}
            </Text>
          </View>
        )}
      </List>
      <List
        testID="classed"
        each={["Plum"]}
        style={{ height: 60 }}
        contentClass="px-4"
        empty={<Text>No plums</Text>}
      >
        {(item) => (
          <View testID="plum" style={{ height: 30 }}>
            <Text>{item()}</Text>
          </View>
        )}
      </List>
      <List testID="none" each={[] as string[]} style={{ height: 60 }} empty={() => <Text>Nothing here</Text>}>
        {(item) => <Text>{item()}</Text>}
      </List>
      <Scroll testID="scroll" style={{ height: 80 }} contentStyle={{ padding: 12 }}>
        <View testID="inner" style={{ height: 20 }} />
      </Scroll>
      <Scroll testID="scroll2" style={{ height: 80 }} contentClass="p-2">
        <View testID="inner2" style={{ height: 20 }} />
      </Scroll>
      <List testID="long" each={Array.from({ length: 30 }, (_, i) => i)} style={{ height: 100 }}>
        {(item) => (
          <View style={{ height: 40 }}>
            <Text>Long {item()}</Text>
          </View>
        )}
      </List>
    </View>
  );
}

export default App;
