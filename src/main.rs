use serde_json::Value;

fn main() {
    // Some JSON input data as a &str. Maybe this comes from the user.
    let data = r#"
        {
            "name": "John Doe",
            "age": 43,
            "phones": [
                "+44 1234567",
                "+44 2345678"
            ]
        }"#;

    // Parse the string of data into serde_json::Value.
    let v: Value = match serde_json::from_str(data) {
        Ok(j) => j,
        Err(err) => {
            panic!("Failed to deserialize: {:?}", err)
        }
    };

    // Access parts of the data by indexing with square brackets.
    println!("Please call {} at the number {}", v["name"], v["phones"][0]);
}
