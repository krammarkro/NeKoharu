import base64
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:4981/openai/v1",
    api_key="not-needed",
)


def data_url(path: str, mime_type: str) -> str:
    with open(path, "rb") as file:
        encoded = base64.b64encode(file.read()).decode()
    return f"data:{mime_type};base64,{encoded}"

response = client.chat.completions.create(
    model="gemini-auto",
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {"url": data_url("3.jpg", "image/jpg")},
                },
            ],
        }
    ],
)

print(response.choices[0].message.content)